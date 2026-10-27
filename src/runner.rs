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
    time::Duration,
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
    crate::coordination::poll(store, None, false)?;
    let mut account = {
        let _lock = store.lock()?;
        store.account(&store.load()?, provider, name)?
    };
    if auto {
        let _ = observations(store, provider)?;
    }
    let mut lease = {
        let _lock = store.lock()?;
        if auto {
            let state = store.load()?;
            let settings = state.auto.get(&provider).cloned().unwrap_or_default();
            if let Some(best) = usage::choose(
                &account.label,
                &entries_from_cache(store, provider)?,
                settings.threshold,
                settings.last_switch,
                now(),
                false,
            ) {
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
    loop {
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
            history.child(child.0.id())?;
        }
        let mut limited = false;
        let mut target = None;
        let mut next_check = 0;
        let status = loop {
            if let Some(terminal) = &mut terminal {
                limited |= terminal.pump(100)?;
            }
            if now() >= next_sync {
                next_sync = now().saturating_add(30);
                if crate::coordination::poll(store, None, false).is_ok() {
                    let _ = crate::refresh::for_run(store, &account);
                    let _ = view.sync(store, &account);
                }
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
                if let Some(next) = usage::choose(
                    &account.label,
                    &entries,
                    settings.threshold,
                    settings.last_switch,
                    now(),
                    false,
                ) {
                    target = Some(next.clone());
                    break stop(&mut child.0)?;
                }
            }
            if auto && limited {
                view.sync(store, &account)?;
                let (entries, settings) = observations(store, provider)?;
                target = usage::choose(
                    &account.label,
                    &entries,
                    settings.threshold,
                    settings.last_switch,
                    now(),
                    true,
                )
                .cloned();
                if target.is_some() {
                    break stop(&mut child.0)?;
                }
            }
            std::thread::sleep(Duration::from_millis(250));
        };
        drop(child);
        if let Some(terminal) = &mut terminal {
            limited |= terminal.pump(0)?;
        }
        view.sync(store, &account)?;
        if !auto {
            return Ok(status.code().unwrap_or(1));
        }
        if status.success() && target.is_none() {
            return Ok(0);
        }
        let did_limit = limited;
        if did_limit {
            usage::mark_limited(store, &account)?;
            let (entries, settings) = observations(store, provider)?;
            target = usage::choose(
                &account.label,
                &entries,
                settings.threshold,
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
            return Ok(status.code().unwrap_or(1));
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
            let next = usage::choose(
                &account.label,
                &entries_from_cache(store, provider)?,
                state
                    .auto
                    .get(&provider)
                    .cloned()
                    .unwrap_or_default()
                    .threshold,
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
        settings.last_switch,
        now(),
        false,
    ) else {
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
