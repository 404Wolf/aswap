use crate::terminal::Terminal;
use crate::{
    Result, Safe, now,
    provider::Provider,
    store::{self, Account, Store},
    usage,
};
use std::{
    path::Path,
    process::{Child, Stdio},
    time::Duration,
};

/// Copy only credentials and account identity; never global user settings.
fn seed(store: &Store, account: &Account, dest: &Path) -> Result<()> {
    let source = store.profile(account.provider, &account.label)?;
    let auth = store::read_private(&source.join(account.provider.auth_file()))?;
    crate::provider::validate_auth(account.provider, &auth)?;
    store::atomic(&dest.join(account.provider.auth_file()), &auth)?;
    if account.provider == Provider::Claude {
        let path = dest.join(".claude.json");
        let mut config: serde_json::Value = if path.exists() {
            store::json_read(&path)?
        } else {
            serde_json::json!({"hasCompletedOnboarding":true})
        };
        let source_path = source.join(".claude.json");
        let saved: serde_json::Value = if source_path.exists() {
            store::json_read(&source_path)?
        } else {
            serde_json::json!({})
        };
        if let Some(identity) = saved.get("oauthAccount") {
            config["oauthAccount"] = identity.clone();
        }
        store::json_write(&path, &config)?;
    } else {
        let path = source.join("config.toml");
        if !dest.join("config.toml").exists() && path.exists() {
            store::atomic(&dest.join("config.toml"), &store::read_private(&path)?)?;
        }
    }
    Ok(())
}
fn sync_auth(store: &Store, account: &Account, session: &Path) -> Result<()> {
    let bytes = store::read_private(&session.join(account.provider.auth_file()))?;
    crate::provider::validate_auth(account.provider, &bytes)?;
    store::atomic(
        &store
            .profile(account.provider, &account.label)?
            .join(account.provider.auth_file()),
        &bytes,
    )
}
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

/// Persistent per-account profiles permit concurrent *different* accounts.
/// Auto sessions keep a stable history directory while changing credentials.
pub fn run(
    store: &Store,
    provider: Provider,
    name: Option<&str>,
    args: &[String],
    auto: bool,
) -> Result<i32> {
    if auto && !args.is_empty() {
        return Err(crate::Error(
            "auto supervision accepts interactive sessions only; pass no provider arguments",
        ));
    }
    let mut account = {
        let _lock = store.lock()?;
        store.account(&store.load()?, provider, name)?
    };
    if auto {
        let (entries, settings) = observations(store, provider)?;
        if let Some(best) = usage::choose(
            &account.label,
            &entries,
            settings.threshold,
            settings.last_switch,
            now(),
            false,
        ) {
            account = best.clone();
        }
    }
    let mut lease = store.lease(&account)?;
    {
        let _lock = store.lock()?;
        store.account(&store.load()?, provider, Some(&account.label))?;
    }
    let profile = store.profile(provider, &account.label)?;
    let runtime;
    let session = if auto {
        let dir = store.root.join("sessions");
        store::private_dir(&dir)?;
        runtime = tempfile::Builder::new()
            .prefix("run-")
            .tempdir_in(dir)
            .safe("cannot create isolated session")?;
        seed(store, &account, runtime.path())?;
        // Keep session history after exit so native resume can find it.
        runtime.keep()
    } else {
        profile
    };
    let mut terminal = if auto { Some(Terminal::open()?) } else { None };
    let mut launch_args = args.to_vec();
    let mut switches = 0;
    loop {
        let mut cmd = provider.command(&session);
        cmd.args(&launch_args);
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
        let mut limited = false;
        let mut target = None;
        let mut next_check = 0;
        let status = loop {
            if let Some(terminal) = &mut terminal {
                limited |= terminal.pump(100)?;
            }
            if let Some(status) = child.0.try_wait().safe("cannot wait for provider")? {
                break status;
            }
            if auto && now() >= next_check {
                next_check = now() + usage::TTL;
                // Refresh-token rotations in this private run become canonical.
                sync_auth(store, &account, &session)?;
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
                sync_auth(store, &account, &session)?;
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
        if !auto {
            return Ok(status.code().unwrap_or(1));
        }
        sync_auth(store, &account, &session)?;
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
        let Some(next) = target else {
            return Ok(status.code().unwrap_or(1));
        };
        drop(lease);
        account = next;
        lease = store.lease(&account)?;
        seed(store, &account, &session)?;
        {
            let _lock = store.lock()?;
            let mut state = store.load()?;
            state.auto.entry(provider).or_default().last_switch = now();
            store.save(&state)?;
        }
        launch_args = match provider {
            Provider::Claude => vec!["--continue".into()],
            Provider::Codex => vec!["resume".into(), "--last".into()],
        };
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
    let _lease = store.lease(account)?;
    if activate_live {
        crate::provider::activate_profile(store, account)?;
    }
    state.active.insert(provider, account.label.clone());
    state.auto.entry(provider).or_default().last_switch = now();
    store.save(&state)?;
    Ok(Some(account.label.clone()))
}
