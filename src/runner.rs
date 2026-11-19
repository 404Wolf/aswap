#[cfg(not(test))]
use crate::now;
#[cfg(test)]
use wake_process_tests::now;

use crate::terminal::Terminal;
use crate::{
    Error, Result, Safe,
    provider::Provider,
    store::{self, Account, Store},
    usage,
};
use std::{
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
mod via;
use monitor::entries_from_cache;
pub use monitor::{observations, tick};
use patience::{COMMIT_PATIENCE, Hiccups, PREPARE_PATIENCE, SUPERVISE_PATIENCE, patiently};
use quiet::{QUIET_STEP, Quiet};
use recovery::{Rejected, Restarts, Wake, quota_fallback, recovery_target, report_quota_fallback};
use rotation::{access_print, rotated_since, rotation_by};
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
        store.account(&store.load()?, provider, name).is_ok()
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
        store.account(&store.load()?, provider, name)?
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
        let (entries, _) = patiently("reading usage", || observations(store, provider))?;
        let _ = models.filter(store, &account.label, entries);
    }
    let mut lease = patiently("reserving the account", || {
        let mut account = account.clone();
        let mut fallback = None;
        let _lock = store.lock()?;
        if pick {
            let state = store.load()?;
            let settings = state.auto.get(&provider).cloned().unwrap_or_default();
            let entries =
                models.filter_cached(store, &account.label, entries_from_cache(store, provider)?);
            if let Some(best) = usage::choose(
                &account.label,
                &entries,
                settings.threshold,
                settings.prefer,
                settings.last_switch,
                now(),
                false,
            )
            .or_else(|| {
                usage::prefer_at_launch(
                    &account.label,
                    &entries,
                    settings.threshold,
                    settings.prefer,
                    now(),
                )
            })
            .or_else(|| {
                how.pick
                    .then(|| {
                        usage::best_known(
                            &account.label,
                            &entries,
                            settings.threshold,
                            settings.prefer,
                            now(),
                        )
                    })
                    .flatten()
            }) {
                account = best.clone();
                fallback = quota_fallback(&account, &entries, &settings, now());
            }
        }
        store.account(&store.load()?, provider, Some(&account.label))?;
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
    let mut cleanup = crate::shared::ViewCleanup::new(&view);
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
    let mut restarts = Restarts::default();
    let mut next_sync = 0;
    let mut syncing: Option<std::thread::JoinHandle<bool>> = None;
    let mut auth_restarts = 0;
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
        cleanup.synced = false;
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
        // SAFETY: only async-signal-safe Linux syscalls run after fork. Close the
        // spawn/record gap: a killed supervisor cannot leave an untracked provider.
        unsafe {
            cmd.pre_exec(move || {
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
        let mut wake = Wake::new(u64::from(std::process::id()));
        let status = loop {
            // Whether the screen showed a limit this tick; for Claude, its
            // transcript decides whether that is live (see `limit_seen`).
            let mut screen_limit = false;
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
                    screen_limit |= terminal.pump_child(wait_ms, &mut child.0)?;
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
                let fresh = (screen_limit && !limited)
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
            limited |= limit_seen(provider, &mut limits, tracker.as_ref(), screen_limit, false);
            if let Some(terminal) = &mut terminal {
                quiet.apply(terminal, tracker.as_ref());
            }
            if let Some(status) = child.0.try_wait().safe("cannot wait for provider")? {
                break status;
            }
            // Quota observations rank future launches, but cannot establish
            // that an interactive provider is idle. Never cancel active tools
            // merely because cached quota crossed a threshold.
            if !expired && !rotated && terminal.as_ref().is_some_and(|t| t.auth_expired) {
                if auth_seen.is_none() {
                    auth_seen = Some(Instant::now());
                    if !settle.is_zero() && syncing.is_none() {
                        syncing = immediate_poll(store);
                    }
                }
                if !limited
                    && !unsupported
                    && auth_restarts < AUTH_RESTARTS
                    && !restarts.capped()
                    && rotated_since(store, &account, launched.as_deref())
                {
                    // The provider called the API in the moment between the
                    // revocation and its view getting the new token. The
                    // account is fine: resume the conversation on it. Until
                    // the conversation is known, retry on every tick.
                    match prepare_live_resume(
                        store,
                        provider,
                        auto_args.as_ref().unwrap(),
                        tracker.as_mut().unwrap(),
                        child.0.id(),
                    ) {
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
                } else if auth_seen.is_some_and(|at| at.elapsed() >= settle) {
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
            }
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
                    let entries = rejected.filter(
                        store,
                        &account.label,
                        settings.threshold,
                        models.filter(store, &account.label, entries),
                    );
                    Ok(recovery_target(&account.label, &entries, &settings, now()).cloned())
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
            let screen_limit = terminal.child_exited()?;
            // Refresh only for Claude limit detection or unresolved held errors.
            let needs_transcript = (!limited && provider.spec().records_limits())
                || (terminal.quiet_auth
                    && !terminal.held_auth.is_empty()
                    && quiet.transcript.is_none());
            if needs_transcript && let Some(tracker) = &mut tracker {
                tracker.transcript_now();
            }
            if !limited {
                limited = limit_seen(provider, &mut limits, tracker.as_ref(), screen_limit, true);
            }
            quiet.apply(terminal, tracker.as_ref());
        }
        cleanup.synced = hiccups
            .note("saving the provider's view", view.sync(store, &account))
            .is_some();
        if !auto {
            return Ok(status.code().unwrap_or(1));
        }
        let auth_failed = terminal.as_ref().is_some_and(|t| t.auth_expired);
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
        if rotated {
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
            hiccups.note("handing over the new token", view.sync(store, &account));
            omit_one_shot(auto_args.as_ref().unwrap(), &mut terminal)?;
            launch_args = resume_args;
            switches += 1;
            restarts.record();
            auth_restarts += 1;
            continue;
        }
        expired |= auth_failed;
        unsupported |= provider.spec().model_catalog()
            && terminal.as_ref().is_some_and(|t| t.model_unsupported);
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
                    let entries = rejected.filter(
                        store,
                        &account.label,
                        settings.threshold,
                        models.filter(store, &account.label, entries),
                    );
                    Ok(recovery_target(&account.label, &entries, &settings, now()).cloned())
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
                        let entries = rejected.filter(
                            store,
                            &account.label,
                            settings.threshold,
                            models.filter(store, &account.label, entries),
                        );
                        Ok(recovery_target(&account.label, &entries, &settings, now()).cloned())
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
            // Eligibility can change after stopping the old provider. A lost
            // target is a normal wait outcome; store failures retain their own
            // retry policy. Neither permits launching a rejected account.
            let commit = || {
                let _lock = store.lock()?;
                let mut state = store.load()?;
                let settings = state.auto.get(&provider).cloned().unwrap_or_default();
                let at = now();
                let entries = rejected.filter_at(
                    store,
                    &account.label,
                    settings.threshold,
                    models.filter_cached(
                        store,
                        &account.label,
                        entries_from_cache(store, provider)?,
                    ),
                    at,
                );
                let Some(next) = recovery_target(&account.label, &entries, &settings, at).cloned()
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
                let fallback = quota_fallback(&next, &entries, &settings, at);
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
/// Whether the provider hit a usage limit. Claude records each limit in its
/// transcript as it happens, so a limit counts only once that entry appears
/// (a redrawn conversation shows old limit notices again but never records
/// them again, and a limit dialog the screen parser can't read still records
/// one). Codex, and a Claude whose transcript is unknown, go by the screen.
fn limit_seen(
    provider: Provider,
    limits: &mut crate::limits::Limits,
    tracker: Option<&crate::session::Tracker>,
    screen: bool,
    fresh: bool,
) -> bool {
    if provider.spec().records_limits() && tracker.is_some() {
        limits.check(tracker, screen, fresh)
    } else {
        screen
    }
}
/// Same-account restarts after a login error that followed a token rotation.
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
