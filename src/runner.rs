#[cfg(not(test))]
use crate::now;
#[cfg(test)]
use wake_process_tests::now;

use crate::terminal::Terminal;
use crate::{
    Result, Safe,
    provider::Provider,
    store::{self, Account, Store},
    switch_gate::SwitchGate,
    usage,
};
use std::{
    collections::hash_map::RandomState,
    fs::File,
    hash::BuildHasher,
    io::{IsTerminal, Read, Seek, SeekFrom, Write},
    os::unix::process::CommandExt,
    path::Path,
    process::{Child, Stdio},
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

fn stop(child: &mut Child) -> Result<std::process::ExitStatus> {
    // SAFETY: the pid belongs to our unreaped child; no other process can reuse it.
    unsafe {
        libc::kill(-(child.id() as libc::pid_t), libc::SIGTERM);
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().safe("cannot wait for provider")? {
            return Ok(status);
        }
        if std::time::Instant::now() >= deadline {
            child.kill().safe("cannot stop provider")?;
            return child.wait().safe("cannot reap provider");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
struct Running(Child, bool);
impl Drop for Running {
    fn drop(&mut self) {
        if self.1 && matches!(self.0.try_wait(), Ok(None)) {
            // SAFETY: this unreaped auto child leads its own process group.
            unsafe {
                libc::kill(-(self.0.id() as libc::pid_t), libc::SIGTERM);
            }
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Every run has its own live lease and access-only authentication view.
/// Auto sessions keep a stable history directory while changing credentials.
pub fn run(
    store: &Store,
    provider: Provider,
    name: Option<&str>,
    args: &[String],
    auto: bool,
) -> Result<i32> {
    let pinned = name;
    // Nothing runs yet: wait out a full disk or a busy store rather than exit.
    store::set_patience(PREPARE_PATIENCE);
    let mut auto_args = if auto {
        Some(crate::session::AutoArgs::parse(provider, args)?)
    } else {
        None
    };
    // A follower whose owner is unreachable, slow or disagrees still launches
    // on its last synced profiles; the in-session poll keeps retrying. Only a
    // follower with no usable profile yet waits for the owner.
    let synced = {
        let _lock = store.lock()?;
        launch_seed(store, &store.load()?, provider, pinned, auto).is_ok()
    };
    if synced {
        drop(background_poll(store));
    } else if let Err(err) = crate::coordination::poll(store, None, false) {
        eprintln!("aswap: {err}; launching with the last synced profiles");
    }
    // Followers: keep the owner's push stream open unless another run or the
    // monitor already does, so a rotated token arrives within about a second.
    crate::coordination::spawn_sync(store);
    let mut account = {
        let _lock = store.lock()?;
        launch_seed(store, &store.load()?, provider, pinned, auto)?
    };
    let mut models = if auto {
        crate::models::Gate::new(provider, args)?
    } else {
        crate::models::Gate::default()
    };
    if auto {
        models.resolve(store, &account)?;
        let (mut entries, settings) = patiently("reading usage", || observations(store, provider))?;
        entries.retain(|(a, _)| settings.allows(&a.label, pinned));
        let _ = models.filter(store, &account.label, entries);
    }
    let mut lease = patiently("reserving the account", || {
        let mut account = account.clone();
        let mut fallback = None;
        let _lock = store.lock()?;
        let state = store.load()?;
        if auto {
            let settings = state.auto.get(&provider).cloned().unwrap_or_default();
            let entries =
                models.filter_cached(store, &account.label, entries_from_cache(store, provider)?);
            trace_candidates("launch", &entries, settings.threshold, now(), None);
            let current = state
                .active
                .get(&provider)
                .filter(|label| pinned.is_none() && !settings.allows(label, None))
                .map_or(account.label.as_str(), String::as_str);
            let at = now();
            let best =
                usage::choose(current, &entries, &settings, at, false, pinned).or_else(|| {
                    pinned
                        .is_none()
                        .then(|| usage::prefer_at_launch(current, &entries, &settings, at))
                        .flatten()
                });
            if let Some(best) = best {
                account = best.clone();
                fallback = quota_fallback(&account, &entries, &settings, at, pinned);
            }
            if !settings.allows(&account.label, pinned) {
                return Err(crate::Error(
                    "no unreserved account available for automatic launch",
                ));
            }
        }
        store.account(&state, provider, Some(&account.label))?;
        Ok((
            account.clone(),
            Arc::new(store.lease_locked(&account)?),
            fallback,
        ))
    })
    .map(|(chosen, lease, fallback)| {
        report_quota_fallback(&chosen, fallback);
        account = chosen;
        lease
    })?;
    let _ = crate::refresh::for_run(store, &account);
    let history_owner = account.label.clone();
    // Retain the history owner's lease even after switching credentials.
    let history_lease = auto.then(|| Arc::clone(&lease));
    {
        let _lock = store.lock()?;
        store.account(&store.load()?, provider, Some(&account.label))?;
    }
    let mut view = patiently("preparing the provider's view", || {
        crate::shared::View::prepare(store, &account)
    })?;
    patiently("recording the session's view", || lease.view(&view.path))?;
    if auto_args.as_ref().is_some_and(|a| a.ambiguous) {
        let help = crate::session::option_help(provider, &view.path, args);
        auto_args = Some(crate::session::AutoArgs::parse_with_help(
            provider, args, &help,
        )?);
    }
    let mut terminal = if auto { Some(Terminal::open()?) } else { None };
    let mut launch_args = auto_args
        .as_ref()
        .map_or_else(|| args.to_vec(), |a| a.initial.clone());
    if models.pin
        && let Some(model) = &models.required
    {
        // Freeze the effective model across account switches and config edits.
        let index = launch_args
            .iter()
            .position(|a| a == "--")
            .unwrap_or(launch_args.len());
        launch_args.splice(index..index, ["--model".into(), model.clone()]);
        let sticky = &mut auto_args.as_mut().expect("auto model arguments").sticky;
        sticky.extend(["--model".into(), model.clone()]);
    }
    let mut tracker = if let Some(args) = &auto_args {
        Some(crate::session::Tracker::new(
            provider,
            &view.home,
            args.id.clone(),
        )?)
    } else {
        None
    };
    let mut rejected = Rejected::default();
    let mut switches = 0;
    let mut gate = SwitchGate::open(store);
    let mut restarts = Restarts::default();
    let mut next_sync = 0;
    let mut syncing: Option<std::thread::JoinHandle<bool>> = None;
    let mut auth_restarts = 0;
    let wake_seed = RandomState::new().hash_one(std::process::id());
    // A follower learns of a rotation from the owner, a moment after the old
    // token stopped working: give it this long to arrive before deciding
    // that a login error means the account itself has expired.
    let settle = if crate::coordination::follower(store).unwrap_or(false) {
        AUTH_SETTLE
    } else {
        Duration::ZERO
    };
    loop {
        // What this provider starts with, to tell a token that rotated under
        // it from one that has really expired.
        let launched = access_print(provider, &view.path.join(provider.auth_file()));
        eprintln!(
            "aswap: using {provider}:{} (supervisor PID {})",
            account.label,
            std::process::id()
        );
        // A restarted provider redraws the conversation it resumes, including
        // the login error that ended the previous one; hold its login errors
        // until the transcript shows which ones are live.
        let resumed = switches > 0;
        let mut quiet = Quiet::new(tracker.as_ref());
        let mut limits = crate::limits::Limits::new(tracker.as_ref());
        let mut cmd = provider.command(&view.path);
        cmd.args(&launch_args);
        let supervisor = std::process::id();
        // SAFETY: only async-signal-safe syscalls run after fork. Close the
        // spawn/record gap: a killed supervisor cannot leave an untracked provider.
        // Without Linux's parent-death signal (macOS), a provider whose
        // supervisor is SIGKILLed keeps running on its own; a supervisor that
        // exits normally still stops it.
        unsafe {
            cmd.pre_exec(move || {
                #[cfg(target_os = "linux")]
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() as u32 != supervisor {
                    libc::_exit(1);
                }
                Ok(())
            });
        }
        let mut child = Running(
            if let Some(terminal) = &mut terminal {
                terminal.spawn(&mut cmd)?
            } else {
                cmd.stdin(Stdio::inherit())
                    .stdout(Stdio::inherit())
                    .stderr(Stdio::inherit())
                    .spawn()
                    .safe("cannot launch provider executable")?
            },
            auto,
        );
        if let Some(terminal) = &mut terminal {
            terminal.quiet_auth = resumed;
        }
        // From here on the provider runs: store trouble costs at most a short
        // wait per tick, and its bookkeeping is retried on later ticks; it
        // never ends the provider.
        store::set_patience(SUPERVISE_PATIENCE);
        let mut hiccups = Hiccups::default();
        hiccups.note("recording the session lease", lease.child(child.0.id()));
        if let Some(history) = &history_lease
            && !Arc::ptr_eq(history, &lease)
        {
            hiccups.note(
                "recording the history lease",
                history.history_child(child.0.id()),
            );
        }
        let mut limited = false;
        let mut expired = false;
        let mut unsupported = false;
        let mut reported_none = false;
        let mut reported_cap = false;
        // How this launch rejected its account: once, so a session that sits
        // on its limited account doesn't escalate the hold every period
        // (`Some(true)` for a limit, which a login or model error replaces).
        let mut rejected_as: Option<bool> = None;
        let mut target = None;
        let mut prepared_resume = None;
        let mut auth_seen: Option<Instant> = None;
        let mut rotated = false;
        let mut reported_resume = false;
        let mut wake = Wake::new(wake_seed);
        let status = loop {
            // Whether the screen showed a limit this tick; for Claude, its
            // transcript decides whether that is live (see `limit_seen`).
            let mut screen_limit = crate::terminal::ScreenLimits::default();
            // Keystrokes and redraws go through as they arrive; the bookkeeping
            // below only runs between these 250ms windows.
            if let Some(terminal) = &mut terminal {
                let deadline = Instant::now() + Duration::from_millis(250);
                loop {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        break;
                    }
                    // While login errors are held, look at the transcript
                    // every QUIET_STEP to know closely when it changed.
                    let check_quiet = terminal.quiet_auth && quiet.needs_check();
                    let step = if check_quiet {
                        left.min(QUIET_STEP)
                    } else {
                        left
                    };
                    // Round up so an idle poll reaches the bookkeeping deadline.
                    let wait_ms = step.as_nanos().div_ceil(1_000_000) as i32;
                    screen_limit.merge(terminal.pump_child_signals(wait_ms, &mut child.0)?);
                    if check_quiet {
                        quiet.check();
                    }
                }
            }
            // The owner exchange runs off this thread so a slow owner never
            // freezes the terminal; apply its profiles once it lands.
            if syncing.as_ref().is_some_and(|s| s.is_finished())
                && syncing.take().unwrap().join().unwrap_or(false)
            {
                let _ = crate::refresh::for_run(store, &account);
                let _ = view.sync(store, &account);
            }
            if syncing.is_none() && now() >= next_sync {
                next_sync = now().saturating_add(30);
                syncing = background_poll(store);
                if syncing.is_none() {
                    let _ = crate::refresh::for_run(store, &account);
                    let _ = view.sync(store, &account);
                }
            }
            // A refresh hands the new token to this view at once; this tick
            // covers what a hand-out skips (an older owner, a missed view).
            if view.auth_changed(store, &account) {
                let _ = view.sync(store, &account);
            }
            if let Some(tracker) = &mut tracker {
                // Refresh while the provider still owns its descriptors. A
                // diagnostic can be followed immediately by provider exit.
                let fresh = (screen_limit.any() && !limited)
                    || terminal.as_ref().is_some_and(|t| {
                        (t.auth_expired && auth_seen.is_none())
                            || (t.model_unsupported && !unsupported)
                    });
                let observed = if fresh {
                    tracker.observe_now(child.0.id())
                } else {
                    tracker.observe(child.0.id())
                };
                hiccups.note("tracking the conversation", observed);
            }
            if !limited {
                limited = limit_seen(
                    store,
                    &account,
                    &mut limits,
                    tracker.as_ref(),
                    screen_limit,
                    terminal.as_ref(),
                    false,
                );
            }
            if let Some(terminal) = &mut terminal {
                quiet.apply(terminal, tracker.as_ref());
            }
            if let Some(status) = child.0.try_wait().safe("cannot wait for provider")? {
                break status;
            }
            // Quota observations rank future launches, but cannot establish
            // that an interactive provider is idle. Never cancel active tools
            // merely because cached quota crossed a threshold.
            if !unsupported
                && provider == Provider::Codex
                && terminal.as_ref().is_some_and(|t| t.model_unsupported)
            {
                unsupported = true;
                models.reject(&account);
                eprintln!(
                    "aswap: current account rejected the session model; looking for a compatible account"
                );
            }
            let auth_failed = !expired && terminal.as_ref().is_some_and(|t| t.auth_expired);
            if auth_failed && auth_seen.is_none() {
                auth_seen = Some(Instant::now());
                if !settle.is_zero() && syncing.is_none() {
                    syncing = immediate_poll(store);
                }
            }
            let quota_rotation = limited
                && !auth_failed
                && !expired
                && provider == Provider::Codex
                && rejected_as.is_none()
                && wake.due(now());
            if !unsupported
                && auth_restarts < AUTH_RESTARTS
                && (quota_rotation || (auth_failed && !limited))
                && !restarts.capped()
                && rotated_since(store, &account, launched.as_deref())
                && (!quota_rotation || current_quota_available(store, &account))
            {
                // The provider errored using an older login. Keep it alive
                // until we know the exact conversation and can hand over auth.
                match prepare_live_resume(
                    store,
                    provider,
                    auto_args.as_ref().unwrap(),
                    tracker.as_mut().unwrap(),
                    child.0.id(),
                )
                .and_then(|args| {
                    view.sync(store, &account)?;
                    Ok(args)
                }) {
                    Ok(args) => {
                        rotated = true;
                        prepared_resume = Some(args);
                        break stop(&mut child.0)?;
                    }
                    Err(err) => {
                        if !reported_resume {
                            eprintln!("aswap: {err}; keeping the current provider running");
                            reported_resume = true;
                        }
                    }
                }
                continue;
            }
            if auth_failed && auth_seen.is_some_and(|at| at.elapsed() >= settle) {
                expired = true;
                hiccups.note(
                    "recording the expired login",
                    usage::mark_auth_expired(store, &account),
                );
                eprintln!(
                    "aswap: login expired for {provider}:{}; looking for another eligible account",
                    account.label
                );
            }
            if auto
                && (limited || expired || unsupported)
                && (expired || unsupported || wake.due(now()))
            {
                let decided = (|| -> Result<Option<Account>> {
                    let limit = !expired && !unsupported;
                    if rejected_as.is_none() || (rejected_as == Some(true) && !limit) {
                        rejected.reject(store, &account, limit)?;
                        if limit {
                            usage::mark_limited(store, &account)?;
                        }
                        rejected_as = Some(limit);
                    }
                    view.sync(store, &account)?;
                    let (entries, settings) = observations(store, provider)?;
                    wake.schedule(&entries, rejected.next_lapse(now()), now());
                    let entries = models.filter(store, &account.label, entries);
                    trace_recovery(
                        store,
                        &account,
                        launched.as_deref(),
                        &entries,
                        &settings,
                        "live recovery",
                        now(),
                    );
                    let entries =
                        rejected.filter(store, &account.label, settings.threshold, entries);
                    Ok(
                        recovery_target(&account.label, &entries, &settings, now(), pinned)
                            .cloned(),
                    )
                })();
                // An unreadable store leaves the decision to the next tick.
                let decided = hiccups.note("choosing another account", decided);
                if let Some(choice) = &decided {
                    target = choice.clone();
                } else {
                    wake.retry(now());
                }
                if target.is_some() && !restarts.capped() {
                    match prepare_live_resume(
                        store,
                        provider,
                        auto_args.as_ref().unwrap(),
                        tracker.as_mut().unwrap(),
                        child.0.id(),
                    ) {
                        Ok(args) => {
                            prepared_resume = Some(args);
                            break stop(&mut child.0)?;
                        }
                        Err(err) => {
                            if !reported_resume {
                                eprintln!("aswap: {err}; keeping the current provider running");
                                reported_resume = true;
                            }
                        }
                    }
                }
                if target.is_some() && restarts.capped() && !reported_cap {
                    eprintln!(
                        "aswap: usage-limit resume is waiting for the eight-restarts-per-hour cap"
                    );
                    reported_cap = true;
                }
                if decided.is_some() && target.is_none() && !reported_none {
                    models.report_none();
                    reported_none = true;
                }
                if limited && !expired && !unsupported && target.is_none() && decided.is_some() {
                    wake.report(provider, &account.label, now());
                }
            }
            if terminal.is_none() {
                std::thread::sleep(Duration::from_millis(250));
            }
        };
        drop(child);
        hiccups.running = false;
        if let Some(terminal) = &mut terminal {
            let screen_limit = terminal.child_exit_signals()?;
            // Refresh only for Claude limit detection or unresolved held errors.
            let needs_transcript = (!limited && provider == Provider::Claude)
                || (terminal.quiet_auth
                    && !terminal.held_auth.is_empty()
                    && quiet.transcript.is_none());
            if needs_transcript && let Some(tracker) = &mut tracker {
                tracker.transcript_now();
            }
            if !limited {
                limited = limit_seen(
                    store,
                    &account,
                    &mut limits,
                    tracker.as_ref(),
                    screen_limit,
                    Some(terminal),
                    true,
                );
            }
            quiet.apply(terminal, tracker.as_ref());
        }
        hiccups.note("saving the provider's view", view.sync(store, &account));
        if !auto {
            return Ok(status.code().unwrap_or(1));
        }
        let auth_failed = terminal.as_ref().is_some_and(|t| t.auth_expired);
        unsupported |=
            provider == Provider::Codex && terminal.as_ref().is_some_and(|t| t.model_unsupported);
        if auth_failed
            && !rotated
            && !expired
            && !limited
            && !unsupported
            && auth_restarts < AUTH_RESTARTS
            && !restarts.capped()
        {
            if !settle.is_zero() && syncing.is_none() {
                syncing = immediate_poll(store);
            }
            let deadline = auth_seen.unwrap_or_else(Instant::now) + settle;
            rotated = rotation_by(store, &account, launched.as_deref(), deadline);
        }
        if limited
            && provider == Provider::Codex
            && !rotated
            && !expired
            && !auth_failed
            && !unsupported
            && rejected_as.is_none()
            && auth_restarts < AUTH_RESTARTS
            && !restarts.capped()
            && rotated_since(store, &account, launched.as_deref())
            && current_quota_available(store, &account)
        {
            rotated = true;
        }
        if rotated
            && patiently("checking account reservation", || {
                let _lock = store.lock()?;
                let state = store.load()?;
                Ok(state
                    .auto
                    .get(&provider)
                    .is_none_or(|settings| settings.allows(&account.label, pinned)))
            })?
        {
            store::set_patience(PREPARE_PATIENCE);
            let resume_args = match prepared_resume.take() {
                Some(args) => args,
                None => prepare_resume(
                    store,
                    provider,
                    auto_args.as_ref().unwrap(),
                    tracker.as_ref().unwrap(),
                )?,
            };
            eprintln!(
                "aswap: {provider}:{} got a new token after this provider started; resuming on the same account",
                account.label
            );
            let _ = crate::refresh::for_run(store, &account);
            patiently("handing over the new token", || view.sync(store, &account))?;
            omit_one_shot(auto_args.as_ref().unwrap(), &mut terminal)?;
            switch_slot(store, &mut gate, &mut terminal)?;
            launch_args = resume_args;
            switches += 1;
            restarts.record();
            auth_restarts += 1;
            continue;
        }
        expired |= auth_failed;
        if unsupported {
            models.reject(&account);
        }
        if status.success() && target.is_none() && !expired && !unsupported {
            return Ok(0);
        }
        expired |= terminal.as_ref().is_some_and(|t| t.auth_expired);
        let did_limit = limited || expired || unsupported;
        // The provider is gone: wait out store trouble rather than give up
        // the session.
        store::set_patience(PREPARE_PATIENCE);
        if did_limit {
            let limit = !expired && !unsupported;
            if rejected_as.is_none() || (rejected_as == Some(true) && !limit) {
                let recorded = hiccups.note(
                    "rejecting the account",
                    rejected.reject(store, &account, limit),
                );
                if recorded.is_some() {
                    rejected_as = Some(limit);
                    if limit {
                        hiccups.note("recording the limit", usage::mark_limited(store, &account));
                    }
                }
            }
            if expired {
                hiccups.note(
                    "recording the expired login",
                    usage::mark_auth_expired(store, &account),
                );
                eprintln!(
                    "aswap: login expired for {provider}:{}; selecting another account",
                    account.label
                );
            }
            if target.is_none() {
                let decided = (|| -> Result<Option<Account>> {
                    let (entries, settings) = observations(store, provider)?;
                    wake.schedule(&entries, rejected.next_lapse(now()), now());
                    let entries = models.filter(store, &account.label, entries);
                    trace_recovery(
                        store,
                        &account,
                        launched.as_deref(),
                        &entries,
                        &settings,
                        "exit recovery",
                        now(),
                    );
                    let entries =
                        rejected.filter(store, &account.label, settings.threshold, entries);
                    Ok(
                        recovery_target(&account.label, &entries, &settings, now(), pinned)
                            .cloned(),
                    )
                })();
                // Keep the decision that stopped the provider if this one fails.
                if let Some(choice) = hiccups.note("choosing another account", decided) {
                    target = choice.or(target);
                }
            }
        }
        // Resume the same interactive history; never replay command arguments.
        // Bound restarts even if provider observations are misleading.
        let quota_wait = limited && !expired && !unsupported;
        if (!did_limit && target.is_none()) || (!quota_wait && restarts.capped()) {
            return Ok(status.code().unwrap_or(1));
        }
        if target.is_none() && !quota_wait {
            models.report_none();
            return Ok(status.code().filter(|code| *code != 0).unwrap_or(1));
        }
        let resume_args = match prepared_resume {
            Some(args) => args,
            None => prepare_resume(
                store,
                provider,
                auto_args.as_ref().unwrap(),
                tracker.as_ref().unwrap(),
            )?,
        };
        // A CLI may exit on a limit instead of leaving a dialog open. Keep
        // the supervisor and exact resume argv until any account recovers.
        let (next, next_lease, fallback) = loop {
            while quota_wait && (target.is_none() || restarts.capped()) {
                if wake.due(now()) {
                    let decided = (|| -> Result<Option<Account>> {
                        if rejected_as.is_none() {
                            rejected.reject(store, &account, true)?;
                            usage::mark_limited(store, &account)?;
                            rejected_as = Some(true);
                        }
                        let (entries, settings) = observations(store, provider)?;
                        wake.schedule(&entries, rejected.next_lapse(now()), now());
                        let entries = models.filter(store, &account.label, entries);
                        trace_recovery(
                            store,
                            &account,
                            launched.as_deref(),
                            &entries,
                            &settings,
                            "waiting recovery",
                            now(),
                        );
                        let entries =
                            rejected.filter(store, &account.label, settings.threshold, entries);
                        Ok(
                            recovery_target(&account.label, &entries, &settings, now(), pinned)
                                .cloned(),
                        )
                    })();
                    if let Some(choice) = hiccups.note("waiting for usable quota", decided) {
                        target = choice;
                    } else {
                        wake.retry(now());
                    }
                }
                if target.is_none() {
                    wake.report(provider, &account.label, now());
                } else if restarts.capped() && !reported_cap {
                    eprintln!(
                        "aswap: usage-limit resume is waiting for the eight-restarts-per-hour cap"
                    );
                    reported_cap = true;
                }
                terminal
                    .as_mut()
                    .expect("auto terminal")
                    .wait_for_restart(250)?;
            }
            // At most one supervisor on this machine switches per interval;
            // the target is chosen below, once this one's turn comes.
            switch_slot(store, &mut gate, &mut terminal)?;
            // Eligibility can change after stopping the old provider. A lost
            // target is a normal wait outcome; store failures retain their own
            // retry policy. Neither permits launching a rejected account.
            let commit = || {
                let _lock = store.lock()?;
                let mut state = store.load()?;
                let settings = state.auto.get(&provider).cloned().unwrap_or_default();
                let at = now();
                let entries = models.filter_cached(
                    store,
                    &account.label,
                    entries_from_cache(store, provider)?,
                );
                trace_recovery(
                    store,
                    &account,
                    launched.as_deref(),
                    &entries,
                    &settings,
                    "commit recovery",
                    at,
                );
                let entries =
                    rejected.filter_at(store, &account.label, settings.threshold, entries, at);
                let Some(next) =
                    recovery_target(&account.label, &entries, &settings, at, pinned).cloned()
                else {
                    return Ok(None);
                };
                let next_lease = if next.label == account.label {
                    Arc::clone(&lease)
                } else if next.label == history_owner {
                    Arc::clone(history_lease.as_ref().expect("auto session history lease"))
                } else {
                    Arc::new(store.lease_locked(&next)?)
                };
                state.auto.entry(provider).or_default().last_switch = at;
                store.save(&state)?;
                rejected.resumed(store, &next, &state.accounts);
                let fallback = quota_fallback(&next, &entries, &settings, at, pinned);
                Ok(Some((next, next_lease, fallback)))
            };
            // Retried with the store lock released between attempts.
            let committed = store::retry_for(
                "switching accounts",
                COMMIT_PATIENCE,
                Duration::from_millis(200),
                commit,
            );
            match committed {
                Ok(Some(next)) => break next,
                // The grant is spent; the next attempt queues again.
                Ok(None) if quota_wait => {
                    eprintln!(
                        "aswap: quota recovery lost eligibility before commit; waiting for verified quota"
                    );
                }
                Err(err) if quota_wait => {
                    hiccups.note::<()>("committing quota recovery", Err(err));
                }
                Ok(None) => return Ok(status.code().filter(|code| *code != 0).unwrap_or(1)),
                Err(err) => return Err(err),
            }
            target = None;
            wake.retry(now());
        };
        report_quota_fallback(&next, fallback);
        if quota_wait {
            eprintln!(
                "aswap: quota available on {provider}:{}; resuming the same session{}",
                next.label,
                if next.label == account.label {
                    " on the current account"
                } else {
                    ""
                }
            );
        }
        drop(lease);
        account = next;
        lease = next_lease;
        let _ = crate::refresh::for_run(store, &account);
        patiently("preparing the provider's view", || {
            view.select(store, &account)
        })?;
        hiccups.note("recording the session's view", lease.view(&view.path));
        omit_one_shot(auto_args.as_ref().unwrap(), &mut terminal)?;
        launch_args = resume_args;
        switches += 1;
        restarts.record();
    }
}

/// Wait for this machine's switch slot (see `switch_gate`) with the provider
/// stopped, keeping the terminal responsive. Every supervisor restart passes
/// here: account switches, returns to a recovered account and resumes on a
/// rotated token. Initial launches do not.
fn switch_slot(
    store: &Store,
    gate: &mut SwitchGate,
    terminal: &mut Option<Terminal>,
) -> Result<()> {
    // An unreadable setting keeps the default rather than end the session.
    let seconds = store
        .lock()
        .and_then(|_lock| store.load())
        .map_or(crate::switch_gate::DEFAULT_INTERVAL_SECONDS, |state| {
            state.switch_interval_seconds
        });
    let interval = Duration::from_secs(seconds.min(store::MAX_SWITCH_INTERVAL));
    gate.acquire(interval, |pause| match terminal {
        Some(terminal) => {
            terminal.wait_for_restart(i32::try_from(pause.as_millis()).unwrap_or(i32::MAX))
        }
        None => {
            std::thread::sleep(pause);
            Ok(())
        }
    })
}

fn launch_seed(
    store: &Store,
    state: &store::State,
    provider: Provider,
    pinned: Option<&str>,
    auto: bool,
) -> Result<Account> {
    let selected = store.account(state, provider, pinned)?;
    let Some(settings) = state.auto.get(&provider) else {
        return Ok(selected);
    };
    if !auto || settings.allows(&selected.label, pinned) {
        return Ok(selected);
    }
    state
        .accounts
        .iter()
        .find(|account| account.provider == provider && settings.allows(&account.label, pinned))
        .cloned()
        .ok_or(crate::Error(
            "no unreserved account available for automatic launch",
        ))
}
/// How long store I/O retries while no provider runs (preparing a launch or a
/// restart), and while one does (then its bookkeeping retries next tick).
const PREPARE_PATIENCE: Duration = Duration::from_secs(600);
const SUPERVISE_PATIENCE: Duration = Duration::from_secs(1);
/// How long store I/O at commit is retried before returning to quota waiting.
const COMMIT_PATIENCE: Duration = Duration::from_secs(60);
/// Retry a step that takes and releases the store lock itself, for as long
/// as no provider runs: under the lock, store I/O gives up within seconds so
/// other sessions are never held up, and the waiting happens here, between
/// attempts, with the lock released.
fn patiently<T>(what: &str, op: impl FnMut() -> Result<T>) -> Result<T> {
    store::retry_for(what, PREPARE_PATIENCE, Duration::from_millis(200), op)
}
/// Store trouble met while supervising a provider: reported once per kind
/// per launch, never fatal to the provider.
struct Hiccups {
    seen: std::collections::BTreeSet<&'static str>,
    /// Whether the provider is still running (it only affects the report).
    running: bool,
}
impl Default for Hiccups {
    fn default() -> Self {
        Self {
            seen: Default::default(),
            running: true,
        }
    }
}
impl Hiccups {
    fn note<T>(&mut self, what: &'static str, result: Result<T>) -> Option<T> {
        match result {
            Ok(value) => Some(value),
            Err(err) => {
                if self.seen.insert(what) {
                    let then = if self.running {
                        "the provider keeps running"
                    } else {
                        "carrying on without it"
                    };
                    eprintln!("aswap: {what} failed ({err}); {then}");
                }
                None
            }
        }
    }
}
/// Claude usually confirms limits in its transcript. Its active limit menu
/// and compaction failure can also be corroborated by the account's quota.
fn limit_seen(
    store: &Store,
    account: &Account,
    limits: &mut crate::limits::Limits,
    tracker: Option<&crate::session::Tracker>,
    screen: crate::terminal::ScreenLimits,
    terminal: Option<&Terminal>,
    fresh: bool,
) -> bool {
    if account.provider != Provider::Claude || tracker.is_none() {
        return screen.notice;
    }
    if limits.check(tracker, screen.notice, fresh) {
        return true;
    }
    limits.check_quota(
        screen,
        || terminal.map(Terminal::input_activity).unwrap_or_default(),
        || usage::cached(store, account).ok(),
        now,
    )
}
/// Same-account restarts after a provider error that followed a token rotation.
const AUTH_RESTARTS: u32 = 3;
/// How long a follower waits for the owner's rotated token after a login error.
const AUTH_SETTLE: Duration = Duration::from_secs(10);
/// A held login error read this long after a restarted provider records its
/// resume prompt is live. Its redraw is written before the prompt and read
/// at once; a live failure needs an API call and a refresh attempt after.
const RESUME_LIVE: Duration = Duration::from_millis(50);
/// Stop holding login errors this long after launch if the transcript never
/// changes; what was held by then is taken for the redraw.
const RESUME_QUIET: Duration = Duration::from_secs(10);
const QUIET_STEP: Duration = Duration::from_millis(25);
/// Maximum transcript data read once when the path is learned after launch.
const QUIET_HISTORY: u64 = 1024 * 1024;

fn first_transcript_write(path: &Path, since: SystemTime) -> Option<SystemTime> {
    let mut file = File::open(path).ok()?;
    let metadata = file.metadata().ok()?;
    let modified = metadata.modified().ok().filter(|at| *at >= since)?;
    let created = metadata.created().ok().filter(|at| *at >= since);
    // A new file has the first write at its head. For an older file, look
    // for the first record from this launch in its recent tail.
    let offset = if created.is_some() {
        0
    } else {
        metadata.len().saturating_sub(QUIET_HISTORY)
    };
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut bytes = Vec::new();
    file.take(QUIET_HISTORY).read_to_end(&mut bytes).ok()?;
    let recorded = bytes
        .split_inclusive(|byte| *byte == b'\n')
        .skip(usize::from(offset != 0))
        .find_map(|line| {
            let entry: serde_json::Value =
                serde_json::from_slice(line.strip_suffix(b"\n")?).ok()?;
            let timestamp = entry.get("timestamp")?.as_str()?;
            let at = SystemTime::from(chrono::DateTime::parse_from_rfc3339(timestamp).ok()?);
            (at >= since).then_some(at)
        });
    // A newly created file can also mark its first write on filesystems
    // with birth times. With neither signal, use the observed mtime.
    Some(recorded.or(created).unwrap_or(modified))
}

/// Tells a restarted provider's redrawn login errors from live ones by when
/// they were read relative to the first change of its transcript since launch,
/// which is the resume prompt being recorded.
struct Quiet {
    since: Instant,
    since_wall: SystemTime,
    transcript: Option<std::path::PathBuf>,
    launch: Option<crate::shared::Stamp>,
    recorded: Option<Instant>,
}
impl Quiet {
    fn new(tracker: Option<&crate::session::Tracker>) -> Self {
        let transcript = tracker.and_then(|t| t.transcript());
        Self {
            since: Instant::now(),
            since_wall: SystemTime::now(),
            launch: transcript.as_deref().and_then(crate::shared::stamp),
            transcript,
            recorded: None,
        }
    }
    fn needs_check(&self) -> bool {
        self.recorded.is_none() && self.transcript.is_some()
    }
    /// Check the cached transcript's stamp without walking the history.
    fn check(&mut self) {
        if self.recorded.is_some() {
            return;
        }
        if let Some(path) = &self.transcript
            && crate::shared::stamp(path) != self.launch
        {
            self.recorded = Some(Instant::now());
        }
    }
    /// Count held login errors that are live, and stop holding once nothing
    /// read from now on can be part of the redraw.
    fn apply(&mut self, terminal: &mut Terminal, tracker: Option<&crate::session::Tracker>) {
        if !terminal.quiet_auth {
            return;
        }
        if self.transcript.is_none()
            && let Some(path) = tracker.and_then(crate::session::Tracker::transcript)
        {
            // The prompt may have been followed by more writes before this
            // lookup. Date it from the first record of this launch.
            self.launch = crate::shared::stamp(&path);
            self.recorded = first_transcript_write(&path, self.since_wall)
                .and_then(|at| at.duration_since(self.since_wall).ok())
                .map(|elapsed| (self.since + elapsed).min(Instant::now()));
            self.transcript = Some(path);
        }
        self.check();
        if let Some(recorded) = self.recorded {
            let live = recorded + RESUME_LIVE;
            terminal.auth_expired |= terminal.held_auth.iter().any(|at| *at > live);
            if Instant::now() > live {
                terminal.quiet_auth = false;
            }
        } else if self.since.elapsed() >= RESUME_QUIET {
            terminal.quiet_auth = false;
        }
        if !terminal.quiet_auth {
            terminal.held_auth.clear();
        }
    }
}

#[cfg(test)]
mod quiet_tests {
    use super::*;
    use crate::session::Tracker;
    use std::{
        fs::{self, FileTimes},
        os::unix::fs::PermissionsExt,
    };

    #[test]
    fn a_redraw_just_after_the_prompt_stays_quiet() {
        let mut quiet = Quiet::new(None);
        let prompt = Instant::now() - 2 * RESUME_LIVE;
        quiet.recorded = Some(prompt);
        let mut terminal = Terminal::open().unwrap();
        terminal.quiet_auth = true;
        terminal.held_auth.push(prompt + Duration::from_millis(20));
        quiet.apply(&mut terminal, None);
        assert!(!terminal.auth_expired);

        terminal.quiet_auth = true;
        terminal
            .held_auth
            .push(prompt + RESUME_LIVE + Duration::from_millis(1));
        quiet.apply(&mut terminal, None);
        assert!(terminal.auth_expired);
    }

    #[test]
    fn chmod_does_not_make_a_redrawn_login_error_live() {
        let home = tempfile::tempdir().unwrap();
        let id = "11111111-2222-4333-8444-555555555555";
        let dir = home.path().join("projects/-synthetic");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{id}.jsonl"));
        fs::write(&path, "{\"type\":\"user\"}\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let tracker = Tracker::new(Provider::Claude, home.path(), Some(id.into())).unwrap();
        let mut quiet = Quiet::new(Some(&tracker));
        let mut terminal = Terminal::open().unwrap();
        terminal.quiet_auth = true;

        std::thread::sleep(Duration::from_millis(5));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        quiet.check();
        std::thread::sleep(RESUME_LIVE + Duration::from_millis(10));
        terminal.held_auth.push(Instant::now());
        quiet.apply(&mut terminal, Some(&tracker));

        assert!(
            !terminal.auth_expired,
            "securing the transcript did not record the prompt"
        );
        assert!(terminal.quiet_auth);
    }

    #[test]
    fn a_login_failure_after_a_late_discovered_prompt_counts_at_exit() {
        let home = tempfile::tempdir().unwrap();
        let id = "11111111-2222-4333-8444-555555555555";
        let mut tracker = Tracker::new(Provider::Claude, home.path(), Some(id.into())).unwrap();
        let mut quiet = Quiet::new(Some(&tracker));
        let mut terminal = Terminal::open().unwrap();
        terminal.quiet_auth = true;
        quiet.since -= Duration::from_secs(2);
        quiet.since_wall -= Duration::from_secs(2);
        let pid = std::process::id();
        tracker.observe(pid).unwrap();
        assert!(tracker.transcript().is_none());

        let dir = home.path().join("projects/-synthetic");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{id}.jsonl"));
        let prompt = Duration::from_millis(500);
        let error = Duration::from_millis(900);
        let since_wall = quiet.since_wall;
        let entry = |kind: &str, at| {
            serde_json::json!({"type": kind,
                "timestamp": chrono::DateTime::<chrono::Utc>::from(since_wall + at)
                    .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)})
        };
        std::fs::write(&path, format!("{}\n", entry("user", prompt))).unwrap();
        tracker.observe(pid).unwrap();
        assert!(tracker.transcript().is_none(), "the retry is still pending");
        quiet.apply(&mut terminal, Some(&tracker));

        let mut file = File::options().append(true).open(&path).unwrap();
        writeln!(file, "{}", entry("assistant", error)).unwrap();
        file.set_times(FileTimes::new().set_modified(quiet.since_wall + error))
            .unwrap();
        drop(file);
        terminal.held_auth.push(quiet.since + prompt);
        terminal
            .held_auth
            .push(quiet.since + error + Duration::from_millis(10));
        tracker.transcript_now();
        quiet.apply(&mut terminal, Some(&tracker));

        assert_eq!(quiet.recorded, Some(quiet.since + prompt));
        assert!(
            terminal.auth_expired,
            "the second error followed the prompt"
        );
    }
}

/// A digest of an auth file's access token: enough to tell that the token
/// changed, without keeping the token itself in memory.
fn access_print(provider: Provider, path: &std::path::Path) -> Option<Vec<u8>> {
    let auth: serde_json::Value = store::json_read(path).ok()?;
    let pointer = if provider == Provider::Claude {
        "/claudeAiOauth/accessToken"
    } else {
        "/tokens/access_token"
    };
    let token = auth.pointer(pointer)?.as_str()?;
    Some(
        ring::digest::digest(&ring::digest::SHA256, token.as_bytes())
            .as_ref()
            .to_vec(),
    )
}
/// Whether the account's token differs from the one the provider started with.
/// Only then is a login error a rotation race rather than an expired account;
/// once a provider started with the current token also fails, it is real.
fn rotated_since(store: &Store, account: &Account, launched: Option<&[u8]>) -> bool {
    let Some(launched) = launched else {
        return false;
    };
    store
        .profile(account.provider, &account.label)
        .is_ok_and(|profile| {
            access_print(
                account.provider,
                &profile.join(account.provider.auth_file()),
            )
            .is_some_and(|now| now != launched)
        })
}

fn current_quota_available(store: &Store, account: &Account) -> bool {
    let Ok(cache) = usage::cached(store, account) else {
        return false;
    };
    let at = now();
    let available = cache.fetched_at != 0
        && at.saturating_sub(cache.fetched_at) < usage::TTL
        && cache.used(at).is_some_and(|used| used < 100.0);
    if trace_selection_enabled() {
        eprintln!(
            "aswap: selection=credential recovery credentials_changed_since_launch=true available={available}"
        );
        trace_candidate(account, &cache, 100.0, at);
    }
    available
}

fn trace_selection_enabled() -> bool {
    std::env::var("ASWAP_TRACE_SELECTION").as_deref() == Ok("1")
}

fn trace_recovery(
    store: &Store,
    account: &Account,
    launched: Option<&[u8]>,
    entries: &[(Account, usage::Cached)],
    settings: &store::Auto,
    stage: &str,
    at: u64,
) {
    if trace_selection_enabled() {
        trace_candidates(
            stage,
            entries,
            settings.threshold,
            at,
            Some(rotated_since(store, account, launched)),
        );
    }
}

fn trace_candidates(
    stage: &str,
    entries: &[(Account, usage::Cached)],
    threshold: f64,
    at: u64,
    changed: Option<bool>,
) {
    if !trace_selection_enabled() {
        return;
    }
    if let Some(changed) = changed {
        eprintln!("aswap: selection={stage} credentials_changed_since_launch={changed}");
    } else {
        eprintln!("aswap: selection={stage}");
    }
    for (account, cache) in entries {
        trace_candidate(account, cache, threshold, at);
    }
}

fn trace_candidate(account: &Account, cache: &usage::Cached, threshold: f64, at: u64) {
    let age = (cache.fetched_at != 0).then(|| at.saturating_sub(cache.fetched_at));
    let reason = match cache.error.as_deref() {
        Some("model-unavailable") => "model-unavailable",
        Some("provider-limit") => "provider-limit",
        Some(_) => "quota-read-failed",
        None if !cache.unverified_limits.is_empty() => "unverified-hard-limit",
        None if age.is_none_or(|age| age > 600) => "missing-or-stale-quota",
        None => match cache.used(at) {
            None => "missing-or-reset-window",
            Some(used) if used >= 100.0 => "quota-exhausted",
            Some(used) if used >= threshold => "at-or-above-soft-threshold",
            Some(_) => "quota-available",
        },
    };
    // Window names and cached error strings can come from stored data. Only
    // emit numeric window fields and the classifications above.
    let windows: Vec<_> = cache
        .usage
        .windows
        .iter()
        .map(|window| {
            (
                window.duration_minutes,
                window.used_percent,
                window.resets_at,
            )
        })
        .collect();
    eprintln!(
        "aswap: candidate={}:{} reason={reason} fetched_at_age_seconds={age:?} windows_minutes_used_percent_resets_at={windows:?}",
        account.provider, account.label,
    );
}

fn rotation_by(
    store: &Store,
    account: &Account,
    launched: Option<&[u8]>,
    deadline: Instant,
) -> bool {
    loop {
        if rotated_since(store, account, launched) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}
fn omit_one_shot(args: &crate::session::AutoArgs, terminal: &mut Option<Terminal>) -> Result<()> {
    if args.one_shot || !std::io::stdin().is_terminal() {
        writeln!(std::io::stderr(), "aswap: automatic restart omitted one-shot initial input/print mode; piped stdin is not replayed").safe("cannot report automatic restart")?;
        if let Some(terminal) = terminal {
            terminal.discard_piped_input();
        }
    }
    Ok(())
}
fn quota_fallback(
    account: &Account,
    entries: &[(Account, usage::Cached)],
    settings: &store::Auto,
    at: u64,
    pinned: Option<&str>,
) -> Option<(f64, f64)> {
    if !settings.allows(&account.label, pinned) {
        return None;
    }
    let used = entries
        .iter()
        .find(|(a, _)| a.provider == account.provider && a.label == account.label)?
        .1
        .used(at)?;
    (used >= settings.threshold && used < 100.0).then_some((used, settings.threshold))
}

fn report_quota_fallback(account: &Account, fallback: Option<(f64, f64)>) {
    if let Some((used, threshold)) = fallback {
        eprintln!(
            "aswap: last-resort quota fallback to {}:{} ({used:.0}% used, soft threshold {threshold:.0}%); using remaining quota instead of waiting for reset",
            account.provider, account.label
        );
    }
}

/// A limited provider can resume on its own account once that account's
/// rejection lapses. Other targets retain the usual forced-switch ranking
/// and below-100% fallback. `Cached::used` requires fresh coverage of every
/// known exhausted window, including in the caches reloaded at commit.
fn recovery_target<'a>(
    current: &str,
    entries: &'a [(Account, usage::Cached)],
    settings: &store::Auto,
    at: u64,
    pinned: Option<&str>,
) -> Option<&'a Account> {
    let usable = |label| {
        entries.iter().find_map(|(account, cache)| {
            (account.label == label
                && settings.allows(&account.label, pinned)
                && cache.used(at).is_some_and(|used| used < 100.0))
            .then_some(account)
        })
    };
    pinned
        .and_then(usable)
        .or_else(|| usage::choose(current, entries, settings, at, true, pinned))
        .or_else(|| usable(current))
}

/// Pure wall-clock schedule, also used with synthetic time in tests. Shared
/// quota caching serializes provider requests; per-supervisor jitter spreads
/// the cache reads and resumed CLI launches after a common reset.
struct Wake {
    next: u64,
    stagger: u64,
    reported: Option<u64>,
}
impl Wake {
    fn new(seed: u64) -> Self {
        Self {
            next: 0,
            stagger: 1 + seed % 30,
            reported: None,
        }
    }
    fn due(&self, at: u64) -> bool {
        at >= self.next
    }
    fn schedule(&mut self, entries: &[(Account, usage::Cached)], hold: Option<u64>, at: u64) {
        let next = entries
            .iter()
            .flat_map(|(_, cache)| {
                cache
                    .reset_times()
                    .chain(std::iter::once(cache.next_poll_at))
            })
            .chain(hold)
            .filter(|when| *when > at)
            .min()
            .unwrap_or(at.saturating_add(300))
            .min(at.saturating_add(300));
        self.next = next.saturating_add(self.stagger);
    }
    fn retry(&mut self, at: u64) {
        self.next = at.saturating_add(6 + self.stagger % 5);
    }
    fn report(&mut self, provider: Provider, current: &str, at: u64) {
        if self.reported != Some(self.next) {
            eprintln!(
                "aswap: {provider}:{current} is waiting on a usage limit; watching all accounts, next quota/reset check in {}s",
                self.next.saturating_sub(at)
            );
            self.reported = Some(self.next);
        }
    }
}

// Run-local rejection survives owner cache replacement and refresh observations.
// Keep identity keys private/in-memory so aliases cannot bounce a session back
// onto credentials that this provider process has already rejected.
// A login or model rejection lasts for the run. A limit rejection lapses (see
// `Lapse`), so one limit, or one misread, can't strand a long session once
// every other account is exhausted too.
#[derive(Default)]
struct Rejected {
    labels: std::collections::BTreeMap<String, Option<Lapse>>,
    identities: std::collections::BTreeMap<String, Option<Lapse>>,
}
/// When a limit rejection lapses: after a hold that doubles with each new
/// limit on the account (15 minutes, then 30, ... up to 4 hours), and only once
/// a reading taken after the rejection is below the auto threshold. A verified
/// window reset permits remaining quota above that soft threshold too. Without
/// a reset, near-full accounts cannot trade a session back and forth on the
/// strength of repeated, misleading quota readings.
#[derive(Clone, Copy)]
struct Lapse {
    at: u64,
    count: u32,
    /// A known window boundary when the account rejected the session. Only a
    /// fresh, successful post-reset observation may release the hold early.
    reset: Option<u64>,
    resumed: bool,
}
const LIMIT_HOLD: u64 = 15 * 60;
const LIMIT_HOLD_MAX: u64 = 4 * 3600;
impl Lapse {
    fn hold(&self) -> u64 {
        (LIMIT_HOLD << self.count.saturating_sub(1).min(8)).min(LIMIT_HOLD_MAX)
    }
    fn lapsed(&self, cache: &usage::Cached, threshold: f64, at: u64) -> bool {
        let reset = self
            .reset
            .is_some_and(|reset| at >= reset && cache.fetched_at >= reset);
        (at >= self.at.saturating_add(self.hold()) || reset)
            && cache.fetched_at > self.at
            && cache
                .used(at)
                .is_some_and(|used| used < if reset { 100.0 } else { threshold })
    }
    /// The same limit seen again while held stays one rejection; a limit
    /// after the rejection lapsed is a new one, held twice as long.
    fn again(previous: Option<Self>, at: u64) -> Self {
        match previous {
            Some(p) if !p.resumed && at < p.at.saturating_add(p.hold()) => p,
            Some(p) => Self {
                at,
                count: p.count.saturating_add(1),
                reset: None,
                resumed: false,
            },
            None => Self {
                at,
                count: 1,
                reset: None,
                resumed: false,
            },
        }
    }
}
impl Rejected {
    /// Consuming a verified reset is one recovery. If this new provider hits
    /// a limit too, begin a new, doubled hold even before the old hold ends.
    fn resumed(&mut self, store: &Store, account: &Account, accounts: &[Account]) {
        for label in std::iter::once(account.label.clone())
            .chain(crate::identity::duplicates(store, account, accounts))
        {
            if let Some(Some(lapse)) = self.labels.get_mut(&label) {
                lapse.resumed = true;
            }
        }
        if let Some(key) = Self::key(store, account)
            && let Some(Some(lapse)) = self.identities.get_mut(&key)
        {
            lapse.resumed = true;
        }
    }
    /// Reject `account` (and its aliases and identity) for the run, or, for
    /// a `limit`, until the rejection lapses.
    fn reject(&mut self, store: &Store, account: &Account, limit: bool) -> Result<()> {
        let at = now();
        let mut keys = vec![account.label.clone()];
        let (identity, reset) = {
            let _lock = store.lock()?;
            keys.extend(crate::identity::duplicates(
                store,
                account,
                &store.load()?.accounts,
            ));
            let path = store
                .profile(account.provider, &account.label)?
                .join("usage-cache.json");
            let cache: usage::Cached = if path.exists() {
                store::json_read(&path)?
            } else {
                usage::Cached::default()
            };
            (
                Self::key(store, account),
                cache.reset_times().filter(|reset| *reset > at).min(),
            )
        };
        let mark = |map: &mut std::collections::BTreeMap<String, Option<Lapse>>, key: String| {
            let next = match map.get(&key) {
                // A login or model rejection is never weakened by a limit.
                Some(None) => None,
                _ if !limit => None,
                previous => {
                    let mut lapse = Lapse::again(previous.copied().flatten(), at);
                    if lapse.at == at {
                        lapse.reset = reset;
                    }
                    Some(lapse)
                }
            };
            map.insert(key, next);
        };
        for key in keys {
            mark(&mut self.labels, key);
        }
        if let Some(key) = identity {
            mark(&mut self.identities, key);
        }
        Ok(())
    }
    fn key(store: &Store, account: &Account) -> Option<String> {
        crate::identity::read(
            account.provider,
            &store.profile(account.provider, &account.label).ok()?,
        )
        .ok()?
        .lock_key()
    }
    fn filter(
        &self,
        store: &Store,
        current: &str,
        threshold: f64,
        entries: Vec<(Account, usage::Cached)>,
    ) -> Vec<(Account, usage::Cached)> {
        self.filter_at(store, current, threshold, entries, now())
    }
    fn next_lapse(&self, at: u64) -> Option<u64> {
        self.labels
            .values()
            .chain(self.identities.values())
            .flatten()
            .map(|lapse| lapse.at.saturating_add(lapse.hold()))
            .filter(|when| *when > at)
            .min()
    }
    fn filter_at(
        &self,
        store: &Store,
        current: &str,
        threshold: f64,
        entries: Vec<(Account, usage::Cached)>,
        at: u64,
    ) -> Vec<(Account, usage::Cached)> {
        let held = |rejection: Option<&Option<Lapse>>, cache: &usage::Cached| match rejection {
            None => false,
            Some(None) => true,
            Some(Some(lapse)) => !lapse.lapsed(cache, threshold, at),
        };
        entries
            .into_iter()
            .filter_map(|(a, mut cache)| {
                let rejected = held(self.labels.get(&a.label), &cache)
                    || Self::key(store, &a)
                        .is_some_and(|key| held(self.identities.get(&key), &cache));
                if rejected && trace_selection_enabled() {
                    eprintln!(
                        "aswap: candidate={}:{} reason=run-rejected",
                        a.provider, a.label
                    );
                }
                if a.label == current {
                    // choose() needs the current entry even while rejected,
                    // but same-account recovery must respect its hold too.
                    if rejected {
                        cache.error = Some("run-rejected".into());
                    }
                    Some((a, cache))
                } else {
                    (!rejected).then_some((a, cache))
                }
            })
            .collect()
    }
}
/// Automatic restarts are bounded per hour rather than per run, so a session
/// that lives for days can still switch when its accounts reset, while a run
/// misled by its observations stops after eight quick restarts.
#[derive(Default)]
struct Restarts(std::collections::VecDeque<Instant>);
const RESTARTS: usize = 8;
const RESTART_WINDOW: Duration = Duration::from_secs(3600);
impl Restarts {
    fn capped(&mut self) -> bool {
        while self
            .0
            .front()
            .is_some_and(|at| at.elapsed() >= RESTART_WINDOW)
        {
            self.0.pop_front();
        }
        self.0.len() >= RESTARTS
    }
    fn record(&mut self) {
        self.0.push_back(Instant::now());
    }
}

fn prepare_live_resume(
    store: &Store,
    provider: Provider,
    args: &crate::session::AutoArgs,
    tracker: &mut crate::session::Tracker,
    pid: u32,
) -> Result<Vec<String>> {
    if provider == Provider::Codex {
        tracker.observe_now(pid)?;
    }
    prepare_resume(store, provider, args, tracker)
}

// Build the exact argv before stopping a live provider. Unknown identity or
// ambiguous option arity must leave the current conversation available.
fn prepare_resume(
    store: &Store,
    provider: Provider,
    args: &crate::session::AutoArgs,
    tracker: &crate::session::Tracker,
) -> Result<Vec<String>> {
    let prompt = {
        let _lock = store.lock()?;
        store
            .load()?
            .auto
            .get(&provider)
            .cloned()
            .unwrap_or_default()
            .resume_prompt
    };
    args.resume(provider, tracker.id()?, prompt.as_deref())
}

/// Poll the refresh owner on a helper thread; true unless the poll failed
/// (an exchange that is not due yet still counts, as the old inline poll did).
/// Followers only: without a configured owner there is nothing to do.
fn background_poll(store: &Store) -> Option<std::thread::JoinHandle<bool>> {
    if !crate::coordination::follower(store).unwrap_or(false) {
        return None;
    }
    let store = Store::open(store.root.clone()).ok()?;
    Some(std::thread::spawn(move || {
        crate::coordination::poll(&store, None, false).is_ok()
    }))
}
/// A follower's poll that skips the 30s schedule: after a login error the
/// owner may hold a rotated token this follower has not received yet.
fn immediate_poll(store: &Store) -> Option<std::thread::JoinHandle<bool>> {
    let store = Store::open(store.root.clone()).ok()?;
    Some(std::thread::spawn(move || {
        crate::coordination::poll(&store, None, true).is_ok()
    }))
}
pub fn observations(
    store: &Store,
    provider: Provider,
) -> Result<(Vec<(Account, usage::Cached)>, store::Auto)> {
    let (accounts, settings) = {
        let _lock = store.lock()?;
        let state = store.load()?;
        (
            state
                .accounts
                .into_iter()
                .filter(|a| a.provider == provider)
                .collect::<Vec<_>>(),
            state.auto.get(&provider).cloned().unwrap_or_default(),
        )
    };
    let entries = accounts
        .into_iter()
        .map(|a| usage::cached(store, &a).map(|c| (a, c)))
        .collect::<Result<Vec<_>>>()?;
    Ok((entries, settings))
}
/// Auto changes the launcher's active account. Existing isolated runs retain
/// their profile; explicit --activate-live swaps the default Claude login.
pub fn tick(store: &Store, provider: Provider, activate_live: bool) -> Result<Option<String>> {
    {
        let _lock = store.lock()?;
        if !store.load()?.auto.get(&provider).is_some_and(|s| s.enabled) {
            return Ok(None);
        }
    }
    let (entries, _) = observations(store, provider)?;
    let _lock = store.lock()?;
    let mut state = store.load()?;
    let settings = state.auto.get(&provider).cloned().unwrap_or_default();
    if !settings.enabled {
        return Ok(None);
    }
    let Some(current) = state.active.get(&provider) else {
        return Ok(None);
    };
    let at = now();
    let Some(account) = usage::choose(current, &entries, &settings, at, false, None)
        .or_else(|| usage::prefer_at_launch(current, &entries, &settings, at))
    else {
        return Ok(None);
    };
    if activate_live {
        crate::provider::activate_profile(store, account)?;
    }
    state.active.insert(provider, account.label.clone());
    state.auto.entry(provider).or_default().last_switch = now();
    store.save(&state)?;
    Ok(Some(account.label.clone()))
}

fn entries_from_cache(store: &Store, provider: Provider) -> Result<Vec<(Account, usage::Cached)>> {
    store
        .load()?
        .accounts
        .into_iter()
        .filter(|a| a.provider == provider)
        .map(|a| {
            let path = store.profile(provider, &a.label)?.join("usage-cache.json");
            let cache = if path.exists() {
                store::json_read(&path)?
            } else {
                usage::Cached::default()
            };
            Ok((a, cache))
        })
        .collect()
}

#[cfg(test)]
mod rejection_tests {
    use super::*;

    fn cache(used: f64, fetched_at: u64) -> usage::Cached {
        usage::Cached {
            usage: usage::Usage {
                windows: vec![usage::Window {
                    name: "seven_day".into(),
                    used_percent: used,
                    resets_at: None,
                    duration_minutes: Some(10080),
                    codex_slot: None,
                }],
                ..Default::default()
            },
            fetched_at,
            ..Default::default()
        }
    }

    #[test]
    fn a_limit_rejection_lapses_after_its_hold_on_a_fresh_reading_below_threshold() {
        let lapse = Lapse {
            at: 10_000,
            count: 1,
            reset: None,
            resumed: false,
        };
        let later = 10_000 + LIMIT_HOLD;
        // Held for the hold, however good the reading.
        assert!(!lapse.lapsed(&cache(5.0, later - 10), 90.0, later - 1));
        assert!(lapse.lapsed(&cache(5.0, later - 10), 90.0, later));
        // Only a reading taken after the rejection, below the threshold, and
        // without an error counts.
        assert!(!lapse.lapsed(&cache(5.0, 9_999), 90.0, 10_500 + LIMIT_HOLD));
        assert!(!lapse.lapsed(&cache(95.0, later), 90.0, later));
        let mut failed = cache(5.0, later);
        failed.error = Some("provider-limit".into());
        assert!(!lapse.lapsed(&failed, 90.0, later));
    }

    #[test]
    fn a_new_limit_after_a_lapse_is_held_twice_as_long() {
        let first = Lapse::again(None, 1_000);
        assert_eq!(
            (first.at, first.count, first.hold()),
            (1_000, 1, LIMIT_HOLD)
        );
        // Seen again every tick while held: still the same rejection.
        let same = Lapse::again(Some(first), 1_000 + LIMIT_HOLD - 1);
        assert_eq!((same.at, same.count), (1_000, 1));
        let second = Lapse::again(Some(first), 1_000 + LIMIT_HOLD);
        assert_eq!((second.count, second.hold()), (2, 2 * LIMIT_HOLD));
        let many = Lapse {
            at: 0,
            count: 30,
            reset: None,
            resumed: false,
        };
        assert_eq!(many.hold(), LIMIT_HOLD_MAX);
    }

    #[test]
    fn a_new_limit_after_an_early_reset_starts_a_new_doubled_hold() {
        let first = Lapse {
            at: 1_000,
            count: 1,
            reset: Some(1_030),
            resumed: true,
        };
        let next = Lapse::again(Some(first), 1_031);
        assert_eq!(
            (next.at, next.count, next.hold()),
            (1_031, 2, 2 * LIMIT_HOLD)
        );
        assert_eq!(next.reset, None);
        assert!(!next.lapsed(&cache(2.0, 1_032), 90.0, 1_032));
    }

    #[test]
    fn accounts_near_the_limit_never_trade_a_session_back_and_forth() {
        let t = tempfile::tempdir().unwrap();
        let store = Store::open(t.path().join("store")).unwrap();
        let account = |label: &str| Account {
            provider: Provider::Claude,
            label: label.into(),
        };
        let at = now();
        let mut rejected = Rejected::default();
        // "a" hit its limit two hours ago (its hold is over); now on "b".
        rejected.labels.insert(
            "a".into(),
            Some(Lapse {
                at: at - 7200,
                count: 1,
                reset: None,
                resumed: false,
            }),
        );
        rejected.labels.insert("expired".into(), None);
        let kept = |entries: Vec<(Account, usage::Cached)>| {
            rejected
                .filter(&store, "b", 90.0, entries)
                .into_iter()
                .map(|(a, _)| a.label)
                .collect::<Vec<_>>()
        };
        // Still near 100%: "a" stays rejected, so "b" can't bounce back to it.
        assert_eq!(
            kept(vec![
                (account("a"), cache(98.0, at)),
                (account("b"), cache(99.0, at))
            ]),
            vec!["b"]
        );
        // "a" reset (or the limit was misread): it is a candidate again.
        assert_eq!(
            kept(vec![
                (account("a"), cache(3.0, at)),
                (account("b"), cache(99.0, at))
            ]),
            vec!["a", "b"]
        );
        // A login rejection never lapses.
        assert_eq!(
            kept(vec![
                (account("expired"), cache(3.0, at)),
                (account("b"), cache(99.0, at))
            ]),
            vec!["b"]
        );
    }

    #[test]
    fn restarts_are_capped_per_hour_not_per_run() {
        let mut restarts = Restarts::default();
        for _ in 0..RESTARTS {
            assert!(!restarts.capped());
            restarts.record();
        }
        assert!(restarts.capped());
        // An hour later the same session may switch again (unless this
        // machine has been up for less than an hour).
        if let Some(old) = Instant::now().checked_sub(RESTART_WINDOW) {
            restarts.0.iter_mut().for_each(|at| *at = old);
            assert!(!restarts.capped());
        }
    }
}

#[cfg(test)]
mod wake_tests {
    use super::*;

    const AT: u64 = 10_000;

    fn entry(
        provider: Provider,
        label: &str,
        used: f64,
        reset: Option<u64>,
        at: u64,
    ) -> (Account, usage::Cached) {
        (
            Account {
                provider,
                label: label.into(),
            },
            usage::Cached {
                usage: usage::Usage {
                    windows: vec![usage::Window {
                        name: "weekly".into(),
                        used_percent: used,
                        resets_at: reset,
                        duration_minutes: Some(10080),
                        codex_slot: None,
                    }],
                    ..Default::default()
                },
                fetched_at: at,
                next_poll_at: at + usage::TTL,
                ..Default::default()
            },
        )
    }

    #[test]
    fn every_selection_path_prefers_the_soonest_weekly_reset_for_both_providers() {
        let settings = store::Auto {
            prefer: store::Prefer::WeeklyReset,
            ..Default::default()
        };
        for provider in [Provider::Claude, Provider::Codex] {
            let entries = [
                entry(provider, "current", 100.0, Some(AT + 4 * 86_400), AT),
                entry(provider, "later", 20.0, Some(AT + 5 * 86_400), AT),
                entry(provider, "sooner", 60.0, Some(AT + 86_400), AT),
                entry(provider, "unknown", 5.0, None, AT),
            ];
            let label = |a: Option<&Account>| a.map(|a| a.label.clone());
            // Launch and the monitor tick: a forced switch off an exhausted account.
            assert_eq!(
                label(usage::choose(
                    "current", &entries, &settings, AT, false, None
                )),
                Some("sooner".into()),
                "{provider}"
            );
            // Quota fallback after a provider limit, and supervisor recovery.
            assert_eq!(
                label(usage::choose(
                    "current", &entries, &settings, AT, true, None
                )),
                Some("sooner".into()),
                "{provider}"
            );
            assert_eq!(
                label(recovery_target("current", &entries, &settings, AT, None)),
                Some("sooner".into()),
                "{provider}"
            );
            // Launch preference and the monitor move off a later-resetting account.
            for current in ["later", "unknown"] {
                assert_eq!(
                    label(usage::prefer_at_launch(current, &entries, &settings, AT)),
                    Some("sooner".into()),
                    "{provider} {current}"
                );
            }
            assert!(usage::prefer_at_launch("sooner", &entries, &settings, AT).is_none());
            // Headroom ranks by most room instead.
            let headroom = store::Auto {
                prefer: store::Prefer::Headroom,
                ..Default::default()
            };
            assert_eq!(
                label(recovery_target("current", &entries, &headroom, AT, None)),
                Some("unknown".into())
            );
        }
    }

    #[test]
    fn recovery_only_uses_a_reserved_label_for_its_pin() {
        let mut entries = [
            entry(Provider::Claude, "held", 95.0, Some(AT + 1000), AT),
            entry(Provider::Claude, "other", 100.0, Some(AT + 1800), AT),
        ];
        let settings = store::Auto {
            reserved: vec!["held".into()],
            ..Default::default()
        };
        assert!(recovery_target("held", &entries, &settings, AT, None).is_none());
        assert!(quota_fallback(&entries[0].0, &entries, &settings, AT, None).is_none());
        for current in ["held", "other"] {
            assert_eq!(
                recovery_target(current, &entries, &settings, AT, Some("held"))
                    .map(|a| a.label.as_str()),
                Some("held")
            );
        }
        entries[0].1.usage.windows[0].used_percent = 100.0;
        entries[1].1.usage.windows[0].used_percent = 95.0;
        assert_eq!(
            recovery_target("held", &entries, &settings, AT, Some("held"))
                .map(|a| a.label.as_str()),
            Some("other")
        );
    }

    #[test]
    fn recovery_rechecks_weekly_quota_from_the_locked_cache_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path().join("store")).unwrap();
        let settings = store::Auto {
            threshold: 90.0,
            prefer: store::Prefer::WeeklyReset,
            ..Default::default()
        };
        let (current, exhausted) = entry(Provider::Claude, "current", 100.0, None, AT);
        let (first, verified) = entry(Provider::Claude, "chatgpt1", 61.0, Some(AT + 6000), AT);
        let (third, _) = entry(Provider::Claude, "chatgpt3", 100.0, None, AT);
        let mut state = store.load().unwrap();
        state.accounts = vec![current.clone(), first.clone(), third.clone()];
        store.save(&state).unwrap();
        let write_cache = |account: &Account, cache: &usage::Cached| {
            let path = store.profile(account.provider, &account.label).unwrap();
            store::json_write(&path.join("usage-cache.json"), cache).unwrap();
        };
        write_cache(&current, &exhausted);
        write_cache(&first, &verified);
        let full = |weekly: f64| {
            usage::parse_claude(&serde_json::json!({
                "five_hour": {"utilization":39},
                "seven_day": {
                    "utilization":weekly,
                    "resets_at":chrono::DateTime::from_timestamp((AT + 3000) as i64, 0)
                        .unwrap().to_rfc3339()
                }
            }))
            .unwrap()
        };
        let partial = usage::parse_claude(&serde_json::json!({
            "five_hour": {"utilization":39}, "seven_day":null
        }))
        .unwrap();
        let select = |rejected: &Rejected| {
            let _lock = store.lock().unwrap();
            let entries = entries_from_cache(&store, Provider::Claude).unwrap();
            let entries = rejected.filter_at(&store, "current", settings.threshold, entries, AT);
            recovery_target("current", &entries, &settings, AT, None).map(|a| a.label.clone())
        };
        let rejected = Rejected::default();
        let usable = usage::updated(usage::Cached::default(), Ok(full(80.0)), AT - 120);
        write_cache(&third, &usable);
        assert_eq!(select(&rejected).as_deref(), Some("chatgpt3"));
        let stale = usage::updated(usage::Cached::default(), Ok(full(80.0)), AT - 601);
        write_cache(&third, &stale);
        assert_eq!(select(&rejected).as_deref(), Some("chatgpt1"));

        let full = usage::updated(usable, Ok(full(100.0)), AT - 60);
        write_cache(&third, &full);
        assert_eq!(select(&rejected).as_deref(), Some("chatgpt1"));
        let mut held = Rejected::default();
        let lapse = Lapse {
            at: AT - 60,
            count: 1,
            reset: Some(AT),
            resumed: false,
        };
        held.labels.insert(third.label.clone(), Some(lapse));
        assert_eq!(select(&held).as_deref(), Some("chatgpt1"));
        let verified_session_reset = usage::updated(full, Ok(partial.clone()), AT);
        write_cache(&third, &verified_session_reset);
        assert!(!lapse.lapsed(&verified_session_reset, 90.0, AT));
        assert_eq!(select(&held).as_deref(), Some("chatgpt1"));
        assert_eq!(select(&rejected).as_deref(), Some("chatgpt1"));

        let no_weekly_history = usage::updated(usage::Cached::default(), Ok(partial), AT);
        write_cache(&third, &no_weekly_history);
        assert_eq!(select(&rejected).as_deref(), Some("chatgpt1"));

        write_cache(&third, &verified_session_reset);
        write_cache(&first, &usage::Cached::default());
        assert_eq!(select(&rejected), None);
    }

    #[test]
    fn wake_watches_every_accounts_weekly_and_session_reset() {
        for provider in [Provider::Claude, Provider::Codex] {
            let mut entries = vec![
                entry(provider, "current", 100.0, Some(AT + 200), AT),
                entry(provider, "other", 100.0, Some(AT + 60), AT),
            ];
            entries[0].1.usage.windows.push(usage::Window {
                name: "session".into(),
                used_percent: 100.0,
                resets_at: Some(AT + 30),
                duration_minutes: Some(300),
                codex_slot: None,
            });
            let mut wake = Wake::new(17);
            wake.schedule(&entries, None, AT);
            assert!((AT + 31..=AT + 60).contains(&wake.next));
            assert!(!wake.due(AT + 29));
            assert!(wake.due(AT + 60));
        }
    }

    #[test]
    fn unknown_and_past_resets_keep_polling_without_spinning() {
        let mut wake = Wake::new(42);
        for reset in [None, Some(AT - 1)] {
            let mut entries = vec![entry(Provider::Claude, "current", 100.0, reset, AT - 600)];
            entries[0].1.next_poll_at = AT - 1;
            wake.schedule(&entries, None, AT);
            assert!((AT + 301..=AT + 330).contains(&wake.next));
            assert!(!wake.due(AT + 1));
        }
        wake.schedule(&[], Some(AT + 20), AT);
        assert!((AT + 21..=AT + 50).contains(&wake.next));
    }

    #[test]
    fn stalled_sessions_stagger_checks_and_observe_normal_cache_refreshes() {
        let entries = vec![entry(Provider::Codex, "current", 100.0, None, AT)];
        let times = (0..32)
            .map(|seed| {
                let mut wake = Wake::new(seed);
                wake.schedule(&entries, None, AT);
                assert!((AT + usage::TTL + 1..=AT + usage::TTL + 30).contains(&wake.next));
                wake.next
            })
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(times.len(), 30);
    }

    #[test]
    fn verified_reset_recovers_remaining_quota_at_or_above_the_soft_threshold() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path().join("store")).unwrap();
        for provider in [Provider::Claude, Provider::Codex] {
            for recovered in ["current", "other"] {
                for used in [90.0, 95.0] {
                    let mut rejected = Rejected::default();
                    rejected.labels.insert(
                        recovered.into(),
                        Some(Lapse {
                            at: AT,
                            count: 3,
                            reset: Some(AT + 30),
                            resumed: false,
                        }),
                    );
                    let mut entries = vec![
                        entry(provider, "current", 100.0, Some(AT + 3600), AT + 30),
                        entry(provider, "other", 100.0, Some(AT + 3600), AT + 30),
                    ];
                    // The five-hour window reset, but the weekly window
                    // still reads at or above the soft switching threshold.
                    let (_, cache) = entries
                        .iter_mut()
                        .find(|(a, _)| a.label == recovered)
                        .unwrap();
                    cache.usage.windows[0].used_percent = used;
                    cache.usage.windows.push(usage::Window {
                        name: "session".into(),
                        used_percent: 0.0,
                        resets_at: Some(AT + 30 + 5 * 3600),
                        duration_minutes: Some(300),
                        codex_slot: None,
                    });
                    let entries = rejected.filter_at(&store, "current", 90.0, entries, AT + 30);
                    assert_eq!(
                        recovery_target(
                            "current",
                            &entries,
                            &store::Auto::default(),
                            AT + 30,
                            None
                        )
                        .map(|a| a.label.as_str()),
                        Some(recovered),
                        "{provider} {recovered} at {used}%"
                    );
                }
            }
        }
    }

    #[test]
    fn only_verified_post_reset_quota_releases_a_hold_early() {
        let lapse = Lapse {
            at: AT,
            count: 3,
            reset: Some(AT + 30),
            resumed: false,
        };
        let (_, mut fresh) = entry(Provider::Claude, "a", 4.0, Some(AT + 3600), AT + 30);
        assert!(lapse.lapsed(&fresh, 90.0, AT + 30));
        fresh.fetched_at = AT + 29;
        assert!(!lapse.lapsed(&fresh, 90.0, AT + 30));
        fresh.fetched_at = AT + 30;
        fresh.usage.windows[0].used_percent = 95.0;
        assert!(lapse.lapsed(&fresh, 90.0, AT + 30));
        fresh.usage.windows[0].used_percent = 100.0;
        assert!(!lapse.lapsed(&fresh, 90.0, AT + 30));
        fresh.usage.windows[0].used_percent = 4.0;
        fresh.error = Some("network".into());
        assert!(!lapse.lapsed(&fresh, 90.0, AT + 30));
    }

    #[test]
    fn the_first_verified_account_wakes_the_session_even_when_it_is_current() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path().join("store")).unwrap();
        for provider in [Provider::Claude, Provider::Codex] {
            for first in ["current", "other"] {
                let mut rejected = Rejected::default();
                for label in ["current", "other"] {
                    rejected.labels.insert(
                        label.into(),
                        Some(Lapse {
                            at: AT,
                            count: 1,
                            reset: Some(AT + if label == first { 30 } else { 60 }),
                            resumed: false,
                        }),
                    );
                }
                let mut entries = vec![
                    entry(provider, "current", 100.0, Some(AT + 30), AT),
                    entry(provider, "other", 100.0, Some(AT + 30), AT),
                ];
                let choose = |entries, at| {
                    recovery_target(
                        "current",
                        &rejected.filter_at(&store, "current", 90.0, entries, at),
                        &store::Auto::default(),
                        at,
                        None,
                    )
                    .map(|a| a.label.clone())
                };
                // A timestamp passing is not evidence of usable quota.
                assert_eq!(choose(entries.clone(), AT + 30), None);
                let recovered = entries.iter_mut().find(|(a, _)| a.label == first).unwrap();
                recovered.1 = entry(provider, first, 3.0, Some(AT + 3600), AT + 30).1;
                assert_eq!(choose(entries, AT + 30).as_deref(), Some(first));
            }
        }
    }

    #[test]
    fn same_account_recovery_keeps_ordinary_holds_and_permanent_rejections() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path().join("store")).unwrap();
        let mut rejected = Rejected::default();
        rejected.labels.insert(
            "current".into(),
            Some(Lapse {
                at: AT,
                count: 1,
                reset: None,
                resumed: false,
            }),
        );
        let choose = |rejected: &Rejected, at| {
            let entries = rejected.filter_at(
                &store,
                "current",
                90.0,
                vec![entry(Provider::Codex, "current", 5.0, None, at)],
                at,
            );
            recovery_target("current", &entries, &store::Auto::default(), at, None)
                .map(|a| a.label.clone())
        };
        assert_eq!(choose(&rejected, AT + 10), None);
        assert_eq!(
            choose(&rejected, AT + LIMIT_HOLD).as_deref(),
            Some("current")
        );
        rejected.labels.insert("current".into(), None);
        assert_eq!(choose(&rejected, AT + LIMIT_HOLD_MAX), None);
    }
}

#[cfg(test)]
#[path = "runner/wake_process_tests.rs"]
mod wake_process_tests;
