use crate::terminal::Terminal;
use crate::{
    Result, Safe, now,
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
        let (entries, _) = patiently("reading usage", || observations(store, provider))?;
        let _ = models.filter(store, &account.label, entries);
    }
    let mut lease = patiently("reserving the account", || {
        let mut account = account.clone();
        let _lock = store.lock()?;
        if auto {
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
            }) {
                account = best.clone();
            }
        }
        store.account(&store.load()?, provider, Some(&account.label))?;
        Ok((account.clone(), Arc::new(store.lease_locked(&account)?)))
    })
    .map(|(chosen, lease)| {
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
        let mut cmd = provider.command(&view.path);
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
        // How this launch rejected its account: once, so a session that sits
        // on its limited account doesn't escalate the hold every period
        // (`Some(true)` for a limit, which a login or model error replaces).
        let mut rejected_as: Option<bool> = None;
        let mut target = None;
        let mut prepared_resume = None;
        let mut auth_seen: Option<Instant> = None;
        let mut rotated = false;
        let mut reported_resume = false;
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
                    let step = if terminal.quiet_auth {
                        left.min(QUIET_STEP)
                    } else {
                        left
                    };
                    screen_limit |= terminal.pump(step.as_millis().max(1) as i32)?;
                    if terminal.quiet_auth {
                        quiet.check(tracker.as_ref(), false);
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
                hiccups.note("tracking the conversation", tracker.observe(child.0.id()));
            }
            limited |= limit_seen(provider, &mut limits, tracker.as_ref(), screen_limit, false);
            if let Some(terminal) = &mut terminal {
                quiet.apply(terminal, tracker.as_ref(), false);
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
                    match prepare_resume(
                        store,
                        provider,
                        auto_args.as_ref().unwrap(),
                        tracker.as_ref().unwrap(),
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
                && provider == Provider::Codex
                && terminal.as_ref().is_some_and(|t| t.model_unsupported)
            {
                unsupported = true;
                models.reject(&account);
                eprintln!(
                    "aswap: current account rejected the session model; looking for a compatible account"
                );
            }
            if auto && (limited || expired || unsupported) {
                let decided = (|| -> Result<Option<Account>> {
                    let limit = !expired && !unsupported;
                    if rejected_as.is_none() || (rejected_as == Some(true) && !limit) {
                        rejected.reject(store, &account, limit)?;
                        rejected_as = Some(limit);
                    }
                    view.sync(store, &account)?;
                    let (entries, settings) = observations(store, provider)?;
                    let entries = rejected.filter(
                        store,
                        &account.label,
                        settings.threshold,
                        models.filter(store, &account.label, entries),
                    );
                    Ok(usage::choose(
                        &account.label,
                        &entries,
                        settings.threshold,
                        settings.prefer,
                        settings.last_switch,
                        now(),
                        true,
                    )
                    .cloned())
                })();
                // An unreadable store leaves the decision to the next tick.
                let decided = hiccups.note("choosing another account", decided);
                if let Some(choice) = &decided {
                    target = choice.clone();
                }
                if target.is_some() && !restarts.capped() {
                    match prepare_resume(
                        store,
                        provider,
                        auto_args.as_ref().unwrap(),
                        tracker.as_ref().unwrap(),
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
                if decided.is_some() && target.is_none() && !reported_none {
                    models.report_none();
                    reported_none = true;
                }
            }
            if terminal.is_none() {
                std::thread::sleep(Duration::from_millis(250));
            }
        };
        drop(child);
        hiccups.running = false;
        if let Some(terminal) = &mut terminal {
            let screen_limit = terminal.pump(0)?;
            limited |= limit_seen(provider, &mut limits, tracker.as_ref(), screen_limit, true);
            // The provider is gone and the tracker will not walk again: look
            // for a transcript it created in its last moments afresh.
            quiet.apply(terminal, tracker.as_ref(), true);
        }
        hiccups.note("saving the provider's view", view.sync(store, &account));
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
        unsupported |=
            provider == Provider::Codex && terminal.as_ref().is_some_and(|t| t.model_unsupported);
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
                hiccups.note(
                    "rejecting the account",
                    rejected.reject(store, &account, limit),
                );
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
            } else if !unsupported {
                hiccups.note("recording the limit", usage::mark_limited(store, &account));
            }
            let decided = (|| -> Result<Option<Account>> {
                let (entries, settings) = observations(store, provider)?;
                let entries = rejected.filter(
                    store,
                    &account.label,
                    settings.threshold,
                    models.filter(store, &account.label, entries),
                );
                Ok(usage::choose(
                    &account.label,
                    &entries,
                    settings.threshold,
                    settings.prefer,
                    settings.last_switch,
                    now(),
                    true,
                )
                .cloned())
            })();
            // Keep the decision that stopped the provider if this one fails.
            if let Some(choice) = hiccups.note("choosing another account", decided) {
                target = choice.or(target);
            }
        }
        // Resume the same interactive history; never replay command arguments.
        // Bound restarts even if provider observations are misleading.
        if (!did_limit && target.is_none()) || restarts.capped() {
            return Ok(status.code().unwrap_or(1));
        }
        let Some(_) = target else {
            models.report_none();
            return Ok(status.code().filter(|code| *code != 0).unwrap_or(1));
        };
        let resume_args = match prepared_resume {
            Some(args) => args,
            None => prepare_resume(
                store,
                provider,
                auto_args.as_ref().unwrap(),
                tracker.as_ref().unwrap(),
            )?,
        };
        // Revalidate, reserve a new per-session lease and commit the decision
        // together. Other sessions using the best account do not exclude it.
        // If that fails, resume on the same account: the session goes on, and
        // a limit there is detected and acted on again.
        let commit = || -> Result<(Account, Arc<crate::live::Lease>)> {
            let _lock = store.lock()?;
            let mut state = store.load()?;
            let settings = state.auto.get(&provider).cloned().unwrap_or_default();
            let next = usage::choose(
                &account.label,
                &rejected.filter(
                    store,
                    &account.label,
                    settings.threshold,
                    models.filter_cached(
                        store,
                        &account.label,
                        entries_from_cache(store, provider)?,
                    ),
                ),
                settings.threshold,
                settings.prefer,
                0,
                now(),
                true,
            )
            .cloned()
            .ok_or(crate::Error(
                "no eligible account after automatic switch decision",
            ))?;
            let next_lease = if next.label == history_owner {
                Arc::clone(history_lease.as_ref().expect("auto session history lease"))
            } else {
                Arc::new(store.lease_locked(&next)?)
            };
            state.auto.entry(provider).or_default().last_switch = now();
            store.save(&state)?;
            Ok((next, next_lease))
        };
        // Retried with the store lock released between attempts.
        let committed = store::retry_for(
            "switching accounts",
            COMMIT_PATIENCE,
            Duration::from_millis(200),
            commit,
        );
        let (next, next_lease) = match committed {
            Ok(next) => next,
            Err(err) => {
                eprintln!(
                    "aswap: cannot switch accounts ({err}); resuming on {provider}:{}",
                    account.label
                );
                (account.clone(), Arc::clone(&lease))
            }
        };
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
/// How long store I/O retries while no provider runs (preparing a launch or a
/// restart), and while one does (then its bookkeeping retries next tick).
const PREPARE_PATIENCE: Duration = Duration::from_secs(600);
const SUPERVISE_PATIENCE: Duration = Duration::from_secs(1);
/// How long a switch is retried before the conversation resumes on the same
/// account instead.
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
    if provider == Provider::Claude && tracker.is_some() {
        limits.check(tracker, screen, fresh)
    } else {
        screen
    }
}
/// Same-account restarts after a login error that followed a token rotation.
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
/// Tells a restarted provider's redrawn login errors from live ones by when
/// they were read relative to the first change of its transcript since launch,
/// which is the resume prompt being recorded.
struct Quiet {
    since: Instant,
    since_wall: std::time::SystemTime,
    transcript: Option<std::path::PathBuf>,
    launch: Option<crate::shared::Stamp>,
    recorded: Option<Instant>,
}
impl Quiet {
    fn new(tracker: Option<&crate::session::Tracker>) -> Self {
        let transcript = tracker.and_then(|t| t.transcript());
        Self {
            since: Instant::now(),
            since_wall: std::time::SystemTime::now(),
            launch: transcript.as_deref().and_then(crate::shared::stamp),
            transcript,
            recorded: None,
        }
    }
    /// One stat; the path comes from the tracker's last walk, or a new one
    /// when `fresh`.
    fn check(&mut self, tracker: Option<&crate::session::Tracker>, fresh: bool) {
        if self.recorded.is_some() {
            return;
        }
        if self.transcript.is_none() {
            // Found only now (the tracker learns files once a tick). One
            // unchanged since before launch is the baseline, so finding it is
            // not a change. One written since launch is the resume prompt
            // being recorded: date that by its mtime, since finding it lags.
            self.transcript = tracker.and_then(|t| {
                if fresh {
                    t.transcript_now()
                } else {
                    t.transcript()
                }
            });
            let Some(path) = &self.transcript else {
                return;
            };
            self.launch = crate::shared::stamp(path);
            if let Ok(modified) = std::fs::metadata(path).and_then(|m| m.modified())
                && modified >= self.since_wall
            {
                let ago = std::time::SystemTime::now()
                    .duration_since(modified)
                    .unwrap_or_default();
                self.recorded = Some(Instant::now().checked_sub(ago).unwrap_or_else(Instant::now));
            }
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
    fn apply(
        &mut self,
        terminal: &mut Terminal,
        tracker: Option<&crate::session::Tracker>,
        fresh: bool,
    ) {
        if !terminal.quiet_auth {
            return;
        }
        self.check(tracker, fresh);
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
/// a reading taken after the rejection is below the auto threshold. Two
/// accounts near 100% therefore never trade a session back and forth: neither
/// reads below the threshold again until its window resets.
#[derive(Clone, Copy)]
struct Lapse {
    at: u64,
    count: u32,
}
const LIMIT_HOLD: u64 = 15 * 60;
const LIMIT_HOLD_MAX: u64 = 4 * 3600;
impl Lapse {
    fn hold(&self) -> u64 {
        (LIMIT_HOLD << self.count.saturating_sub(1).min(8)).min(LIMIT_HOLD_MAX)
    }
    fn lapsed(&self, cache: &usage::Cached, threshold: f64, at: u64) -> bool {
        at >= self.at.saturating_add(self.hold())
            && cache.fetched_at > self.at
            && cache.used(at).is_some_and(|used| used < threshold)
    }
    /// The same limit seen again while held stays one rejection; a limit
    /// after the rejection lapsed is a new one, held twice as long.
    fn again(previous: Option<Self>, at: u64) -> Self {
        match previous {
            Some(p) if at < p.at.saturating_add(p.hold()) => p,
            Some(p) => Self {
                at,
                count: p.count.saturating_add(1),
            },
            None => Self { at, count: 1 },
        }
    }
}
impl Rejected {
    /// Reject `account` (and its aliases and identity) for the run, or, for
    /// a `limit`, until the rejection lapses.
    fn reject(&mut self, store: &Store, account: &Account, limit: bool) -> Result<()> {
        let at = now();
        let mut keys = vec![account.label.clone()];
        let identity = {
            let _lock = store.lock()?;
            keys.extend(crate::identity::duplicates(
                store,
                account,
                &store.load()?.accounts,
            ));
            Self::key(store, account)
        };
        let mark = |map: &mut std::collections::BTreeMap<String, Option<Lapse>>, key: String| {
            let next = match map.get(&key) {
                // A login or model rejection is never weakened by a limit.
                Some(None) => None,
                _ if !limit => None,
                previous => Some(Lapse::again(previous.copied().flatten(), at)),
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
        let at = now();
        let held = |rejection: Option<&Option<Lapse>>, cache: &usage::Cached| match rejection {
            None => false,
            Some(None) => true,
            Some(Some(lapse)) => !lapse.lapsed(cache, threshold, at),
        };
        entries
            .into_iter()
            .filter(|(a, cache)| {
                a.label == current
                    || (!held(self.labels.get(&a.label), cache)
                        && Self::key(store, a)
                            .is_none_or(|key| !held(self.identities.get(&key), cache)))
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
    let Some(account) = usage::choose(
        current,
        &entries,
        settings.threshold,
        settings.prefer,
        settings.last_switch,
        now(),
        false,
    )
    .or_else(|| {
        usage::prefer_at_launch(
            current,
            &entries,
            settings.threshold,
            settings.prefer,
            now(),
        )
    }) else {
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
        let many = Lapse { at: 0, count: 30 };
        assert_eq!(many.hold(), LIMIT_HOLD_MAX);
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
