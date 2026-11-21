#[cfg(not(test))]
use crate::now;
#[cfg(test)]
use wake_process_tests::now;

use crate::terminal::Terminal;
use crate::{
    Error, Result, Safe,
    provider::Provider,
    store::{self, Account, Store},
    switch_gate::SwitchGate,
    usage,
};
use std::{
    collections::hash_map::RandomState,
    hash::BuildHasher,
    io::{IsTerminal, Write},
    os::unix::process::CommandExt,
    process::{Child, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

mod monitor;
mod patience;
mod quiet;
mod recovery;
mod rotation;
mod trace;
mod via;
use monitor::entries_from_cache;
pub use monitor::{observations, tick};
use patience::{COMMIT_PATIENCE, Hiccups, PREPARE_PATIENCE, SUPERVISE_PATIENCE, patiently};
use quiet::{QUIET_STEP, Quiet};
use recovery::{Rejected, Restarts, Wake, quota_fallback, recovery_target, report_quota_fallback};
use rotation::{access_print, current_quota_available, rotated_since, rotation_by};
use trace::{trace_candidates, trace_recovery};
pub use via::{PI_BIN, Via, ViaError};

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

/// How `aswap run` launches a provider.
#[derive(Clone, Copy, Default)]
pub struct Launch {
    /// Supervise in a PTY: at a usage limit or an expired login, resume the
    /// same conversation on another account.
    pub auto: bool,
    /// Pick the account by quota at launch, as `auto` does, but run with
    /// plain inherited stdio and no supervision (`claude -p` pipes).
    pub pick: bool,
    /// Run another program in front of the provider.
    pub via: Option<Via>,
}
/// Why a run can't start in a working directory that no longer exists.
pub const GONE_CWD: &str = "the current directory no longer exists (was it deleted, e.g. a removed worktree?); cd into an existing directory and run again";
/// Every run has its own live lease and access-only authentication view.
/// Auto sessions keep a stable history directory while changing credentials.
pub fn run(
    store: &Store,
    provider: Provider,
    name: Option<&str>,
    args: &[String],
    auto: bool,
) -> Result<i32> {
    launch(
        store,
        provider,
        name,
        args,
        Launch {
            auto,
            ..Launch::default()
        },
    )
}
pub fn launch(
    store: &Store,
    provider: Provider,
    name: Option<&str>,
    args: &[String],
    how: Launch,
) -> Result<i32> {
    // The provider inherits this directory, and Codex's model lookup needs it:
    // one deleted under the shell (a removed worktree) fails here, plainly.
    std::env::current_dir().map_err(|_| crate::Error(GONE_CWD))?;
    let auto = how.auto;
    if let Some(via) = how.via {
        via.check(provider)?;
        if auto || how.pick {
            return Err(Error(
                "--via picks an account per call without --account; it takes no --auto or --pick",
            ));
        }
        if name.is_none() {
            return via::per_call(store, provider, via, args);
        }
    }
    if how.pick && (auto || name.is_some()) {
        return Err(Error(
            "--pick chooses the account; it takes no --account or --auto",
        ));
    }
    // Whether the account is chosen by quota at launch.
    let pick = auto || how.pick;
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
    }
    if pick {
        let (mut entries, settings) = patiently("reading usage", || observations(store, provider))?;
        entries.retain(|(a, _)| settings.allows(&a.label, pinned));
        let _ = models.filter(store, &account.label, entries);
    }
    let mut lease = patiently("reserving the account", || {
        let mut account = account.clone();
        let mut fallback = None;
        let _lock = store.lock()?;
        let state = store.load()?;
        if pick {
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
            let best = usage::choose(current, &entries, &settings, at, false, pinned)
                .or_else(|| {
                    pinned
                        .is_none()
                        .then(|| usage::prefer_at_launch(current, &entries, &settings, at))
                        .flatten()
                })
                .or_else(|| {
                    // `--pick` serves one call: a stale but error-free reading
                    // still tells a live account from a dead one.
                    how.pick
                        .then(|| usage::best_known(current, &entries, &settings, at, pinned))
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
    let mut cleanup = how
        .pick
        .then(|| crate::shared::ViewCleanup::new(store, &view));
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
        let mut cmd = match how.via {
            Some(via) => via.command(provider, &view.path),
            None => provider.command(&view.path),
        };
        if let Some(tracker) = &mut tracker
            && provider
                .spec()
                .report_session(store, &view.report_path(), &mut cmd)?
        {
            tracker.report_to(view.report_path());
        }
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
                && provider.spec().model_catalog()
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
                && provider.spec().limit_from_stale_login()
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
            let needs_transcript = (!limited && provider.spec().records_limits())
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
        let saved = hiccups
            .note("saving the provider's view", view.sync(store, &account))
            .is_some();
        if let Some(cleanup) = &mut cleanup {
            cleanup.synced = saved;
        }
        if !auto {
            return Ok(status.code().unwrap_or(1));
        }
        let auth_failed = terminal.as_ref().is_some_and(|t| t.auth_expired);
        unsupported |= provider.spec().model_catalog()
            && terminal.as_ref().is_some_and(|t| t.model_unsupported);
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
            && provider.spec().limit_from_stale_login()
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
/// Whether the provider hit a usage limit. A provider that records limits
/// in its transcript (`Spec::records_limits`) is believed only once that
/// entry appears: a redrawn conversation shows old notices again but never
/// records them again. Claude's active limit menu and failed compaction can
/// also be corroborated by the account's fresh quota. Other providers, and
/// one whose transcript is unknown, go by the screen.
fn limit_seen(
    store: &Store,
    account: &Account,
    limits: &mut crate::limits::Limits,
    tracker: Option<&crate::session::Tracker>,
    screen: crate::terminal::ScreenLimits,
    terminal: Option<&Terminal>,
    fresh: bool,
) -> bool {
    if !account.provider.spec().records_limits() || tracker.is_none() {
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
fn omit_one_shot(args: &crate::session::AutoArgs, terminal: &mut Option<Terminal>) -> Result<()> {
    if args.one_shot || !std::io::stdin().is_terminal() {
        writeln!(std::io::stderr(), "aswap: automatic restart omitted one-shot initial input/print mode; piped stdin is not replayed").safe("cannot report automatic restart")?;
        if let Some(terminal) = terminal {
            terminal.discard_piped_input();
        }
    }
    Ok(())
}
/// `prepare_resume` for a provider that is still running: a provider whose
/// session only its writer proves (`Spec::session_by_writer`) is rescanned
/// first, so a switch made since the last periodic scan is resumed.
fn prepare_live_resume(
    store: &Store,
    provider: Provider,
    args: &crate::session::AutoArgs,
    tracker: &mut crate::session::Tracker,
    pid: u32,
) -> Result<Vec<String>> {
    if provider.spec().session_by_writer() {
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

#[cfg(test)]
#[path = "runner/wake_process_tests.rs"]
mod wake_process_tests;
