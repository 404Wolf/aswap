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
    crate::coordination::spawn_watch(store);
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
        let (entries, _) = observations(store, provider)?;
        let _ = models.filter(store, &account.label, entries);
    }
    let mut lease = {
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
        Arc::new(store.lease_locked(&account)?)
    };
    let _ = crate::refresh::for_run(store, &account);
    let history_owner = account.label.clone();
    // Retain the history owner's lease even after switching credentials.
    let history_lease = auto.then(|| Arc::clone(&lease));
    {
        let _lock = store.lock()?;
        store.account(&store.load()?, provider, Some(&account.label))?;
    }
    let mut view = crate::shared::View::prepare(store, &account)?;
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
    let mut switches = 0;
    let mut next_sync = 0;
    let mut syncing: Option<std::thread::JoinHandle<bool>> = None;
    loop {
        eprintln!(
            "aswap: using {provider}:{} (supervisor PID {})",
            account.label,
            std::process::id()
        );
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
        lease.child(child.0.id())?;
        if let Some(history) = &history_lease
            && !Arc::ptr_eq(history, &lease)
        {
            history.history_child(child.0.id())?;
        }
        let mut limited = false;
        let mut expired = false;
        let mut unsupported = false;
        let mut reported_none = false;
        let mut target = None;
        let mut next_check = 0;
        let status = loop {
            // Keystrokes and redraws go through as they arrive; the bookkeeping
            // below only runs between these 250ms windows.
            if let Some(terminal) = &mut terminal {
                let deadline = Instant::now() + Duration::from_millis(250);
                loop {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        break;
                    }
                    limited |= terminal.pump(left.as_millis().max(1) as i32)?;
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
            // The owner revokes the old access token as soon as it rotates, so
            // copy a changed profile token into the view on the next tick.
            if view.auth_changed(store, &account) {
                let _ = view.sync(store, &account);
            }
            if let Some(tracker) = &mut tracker {
                tracker.observe(child.0.id())?;
            }
            if let Some(status) = child.0.try_wait().safe("cannot wait for provider")? {
                break status;
            }
            if auto && now() >= next_check {
                next_check = now() + usage::TTL;
                // Refresh-token rotations in this private run become canonical.
                view.sync(store, &account)?;
                let (entries, settings) = observations(store, provider)?;
                let entries = models.filter(store, &account.label, entries);
                if let Some(next) = usage::choose(
                    &account.label,
                    &entries,
                    settings.threshold,
                    settings.prefer,
                    settings.last_switch,
                    now(),
                    false,
                ) {
                    target = Some(next.clone());
                    break stop(&mut child.0)?;
                }
                if !reported_none
                    && entries
                        .iter()
                        .find(|(a, _)| a.label == account.label)
                        .and_then(|(_, c)| c.used(now()))
                        .is_some_and(|used| used >= settings.threshold)
                {
                    models.report_none();
                    reported_none = true;
                }
            }
            if !expired && terminal.as_ref().is_some_and(|t| t.auth_expired) {
                expired = true;
                usage::mark_auth_expired(store, &account)?;
                eprintln!(
                    "aswap: login expired for {provider}:{}; looking for another eligible account",
                    account.label
                );
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
                view.sync(store, &account)?;
                let (entries, settings) = observations(store, provider)?;
                let entries = models.filter(store, &account.label, entries);
                target = usage::choose(
                    &account.label,
                    &entries,
                    settings.threshold,
                    settings.prefer,
                    settings.last_switch,
                    now(),
                    true,
                )
                .cloned();
                if target.is_some() {
                    break stop(&mut child.0)?;
                }
                if !reported_none {
                    models.report_none();
                    reported_none = true;
                }
            }
            if terminal.is_none() {
                std::thread::sleep(Duration::from_millis(250));
            }
        };
        drop(child);
        if let Some(terminal) = &mut terminal {
            limited |= terminal.pump(0)?;
        }
        view.sync(store, &account)?;
        if !auto {
            return Ok(status.code().unwrap_or(1));
        }
        expired |= terminal.as_ref().is_some_and(|t| t.auth_expired);
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
        if did_limit {
            if expired {
                usage::mark_auth_expired(store, &account)?;
                eprintln!(
                    "aswap: login expired for {provider}:{}; selecting another account",
                    account.label
                );
            } else if !unsupported {
                usage::mark_limited(store, &account)?;
            }
            let (entries, settings) = observations(store, provider)?;
            let entries = models.filter(store, &account.label, entries);
            target = usage::choose(
                &account.label,
                &entries,
                settings.threshold,
                settings.prefer,
                settings.last_switch,
                now(),
                true,
            )
            .cloned();
        }
        // Resume the same interactive history; never replay command arguments.
        // Bound restarts even if provider observations are misleading.
        if (!did_limit && target.is_none()) || switches >= 8 {
            return Ok(status.code().unwrap_or(1));
        }
        let Some(_) = target else {
            models.report_none();
            return Ok(status.code().filter(|code| *code != 0).unwrap_or(1));
        };
        let exact_id = tracker
            .as_ref()
            .expect("auto session tracker")
            .id()?
            .to_owned();
        // Revalidate, reserve a new per-session lease and commit the decision
        // together. Other sessions using the best account do not exclude it.
        let (next, next_lease) = {
            let _lock = store.lock()?;
            let mut state = store.load()?;
            let settings = state.auto.get(&provider).cloned().unwrap_or_default();
            let next = usage::choose(
                &account.label,
                &models.filter_cached(store, &account.label, entries_from_cache(store, provider)?),
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
            (next, next_lease)
        };
        drop(lease);
        account = next;
        lease = next_lease;
        let _ = crate::refresh::for_run(store, &account);
        view.select(store, &account)?;
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
        if auto_args.as_ref().is_some_and(|a| a.one_shot) || !std::io::stdin().is_terminal() {
            writeln!(std::io::stderr(), "aswap: automatic restart omitted one-shot initial input/print mode; piped stdin is not replayed").safe("cannot report automatic restart")?;
            if let Some(terminal) = &mut terminal {
                terminal.discard_piped_input();
            }
        }
        launch_args = auto_args.as_ref().expect("auto arguments").resume(
            provider,
            &exact_id,
            prompt.as_deref(),
        )?;
        switches += 1;
    }
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
