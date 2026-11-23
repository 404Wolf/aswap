use crate::{
    Error, Result, Safe,
    store::{self, Account, Store},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fmt,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(
    Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize, clap::ValueEnum,
)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Claude,
    Codex,
    /// Google's Antigravity CLI, signed in with a personal Google account
    /// (see `crate::agy`).
    Agy,
}
impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Agy => "agy",
        }
    }
    pub fn auth_file(self) -> &'static str {
        match self {
            Self::Claude => ".credentials.json",
            Self::Codex => "auth.json",
            Self::Agy => crate::agy::AUTH_FILE,
        }
    }
    pub fn env_var(self) -> &'static str {
        match self {
            Self::Claude => "CLAUDE_CONFIG_DIR",
            Self::Codex => "CODEX_HOME",
            // agy takes its root as a flag (`--gemini_dir`, see `command`);
            // this variable is the one aswap sets for its plugin secrets.
            Self::Agy => "XDG_CONFIG_HOME",
        }
    }
    pub fn current_home(self) -> Result<PathBuf> {
        if self == Self::Agy {
            let p = crate::agy::current_home()?;
            store::no_links(&p)?;
            return Ok(p);
        }
        let p = std::env::var_os(self.env_var())
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or(home()?.join(match self {
                Self::Claude => ".claude",
                Self::Codex => ".codex",
                Self::Agy => unreachable!("handled above"),
            }));
        store::no_links(&p)?;
        Ok(p)
    }
    /// `ASWAP_CLAUDE_BIN`/`ASWAP_CODEX_BIN` name the real provider binary, so a
    /// `claude` on PATH may itself be a wrapper around `aswap run claude`.
    pub fn bin_var(self) -> &'static str {
        match self {
            Self::Claude => "ASWAP_CLAUDE_BIN",
            Self::Codex => "ASWAP_CODEX_BIN",
            Self::Agy => "ASWAP_AGY_BIN",
        }
    }
    pub fn command(self, profile: &Path) -> Command {
        let bin = std::env::var_os(self.bin_var())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| self.as_str().into());
        let mut cmd = Command::new(&bin);
        // PATH may contain a provider wrapper that invokes aswap. Tell it this
        // launch is already supervised so it executes the native CLI directly.
        cmd.env(self.bin_var(), &bin);
        if self == Self::Agy {
            // The view is agy's whole root; its plugin secrets live in the
            // view too, and without a session bus the login stays a file.
            cmd.arg(format!("--gemini_dir={}", profile.display()));
            cmd.env("XDG_CONFIG_HOME", profile.join(crate::agy::CONFIG));
            cmd.env("AGY_CLI_DISABLE_AUTO_UPDATE", "true");
            for key in crate::agy::SCRUBBED {
                cmd.env_remove(key);
            }
        } else {
            cmd.env(self.env_var(), profile);
        }
        // Prevent inherited credentials/config overrides from defeating account isolation.
        for key in [
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR",
            "CLAUDE_SECURESTORAGE_CONFIG_DIR",
            "OPENAI_API_KEY",
            "OPENAI_BASE_URL",
            "CODEX_API_KEY",
            "CODEX_AUTH_JSON",
            "CODEX_REMOTE_TOKEN",
            "CODEX_REMOTE_URL",
            "CODEX_ACCESS_TOKEN",
            "CODEX_REFRESH_TOKEN_URL_OVERRIDE",
            "CODEX_APP_SERVER_LOGIN_CLIENT_ID",
            "CLAUDE_CODE_CUSTOM_OAUTH_URL",
            "CLAUDE_CODE_OAUTH_CLIENT_ID",
            "ANTHROPIC_BASE_URL",
        ] {
            cmd.env_remove(key);
        }
        if self == Self::Codex {
            cmd.args(["-c", "cli_auth_credentials_store=\"file\""]);
        }
        // SAFETY: umask is async-signal-safe; it changes only the forked child.
        unsafe {
            cmd.pre_exec(|| {
                libc::umask(0o077);
                Ok(())
            });
        }
        cmd
    }
}
impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
pub fn home() -> Result<PathBuf> {
    let p = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or(Error("HOME must be set"))?;
    if !p.is_absolute() {
        return Err(Error("HOME must be absolute"));
    }
    Ok(p)
}
pub(crate) fn global_config(profile: &Path, default: bool) -> Result<PathBuf> {
    let legacy = profile.join(".config.json");
    store::no_links(&legacy)?;
    let global = if default
        && std::env::var_os("CLAUDE_CONFIG_DIR")
            .filter(|s| !s.is_empty())
            .is_none()
    {
        home()?.join(".claude.json")
    } else {
        profile.join(".claude.json")
    };
    if legacy.is_file()
        && (legacy
            .metadata()
            .safe("cannot inspect Claude config")?
            .len()
            > 0
            || !global.exists())
    {
        Ok(legacy)
    } else {
        Ok(global)
    }
}
fn auth_valid(provider: Provider, value: &Value) -> bool {
    let string = |v: Option<&Value>| v.and_then(Value::as_str).is_some_and(|s| !s.is_empty());
    match provider {
        Provider::Claude => string(value.pointer("/claudeAiOauth/accessToken")),
        Provider::Codex => {
            string(value.pointer("/tokens/access_token")) || string(value.get("OPENAI_API_KEY"))
        }
        Provider::Agy => crate::agy::auth_valid(value),
    }
}
pub fn validate_auth(provider: Provider, bytes: &[u8]) -> Result<()> {
    let value: Value =
        serde_json::from_slice(bytes).safe("invalid credential JSON; contents redacted")?;
    if !auth_valid(provider, &value) {
        return Err(Error(
            "unsupported or missing credentials; use a file-based provider login first",
        ));
    }
    Ok(())
}
pub fn add(store: &Store, provider: Provider, name: &str) -> Result<Account> {
    store::label(name)?;
    let _lock = store.lock()?;
    let mut state = store.load()?;
    if state
        .accounts
        .iter()
        .any(|a| a.provider == provider && a.label == name)
    {
        return Err(Error("account already exists"));
    }
    let source = provider.current_home()?;
    if provider == Provider::Agy {
        // An older aswap can't read state.json once it holds an agy account.
        let others = crate::agy::other_builds(&store.root);
        if !others.is_empty() {
            eprintln!("aswap: other aswap builds are running: pids {others:?}");
            return Err(Error(
                "restart every aswap process on this build before adding an agy account",
            ));
        }
    }
    let bytes = if provider == Provider::Agy {
        // agy rewrites its login in place: read it only once it parses, and
        // never write to it.
        let login = crate::agy::read_settled(&source.join(provider.auth_file()))?;
        serde_json::to_vec(&login).safe("cannot encode the agy login")?
    } else {
        store::read_private(&source.join(provider.auth_file()))?
    };
    validate_auth(provider, &bytes)?;
    let target = store.profile(provider, name)?;
    if target.exists() {
        return Err(Error("profile already exists; refusing to overwrite"));
    }
    store::private_dir(&target)?;
    let result = (|| {
        // A pending login left under this label by an earlier account is stale.
        crate::refresh::discard_pending(
            store,
            &Account {
                provider,
                label: name.to_owned(),
            },
        );
        store::atomic(&target.join(provider.auth_file()), &bytes)?;
        if provider == Provider::Claude {
            let path = global_config(&source, true)?;
            if path.exists() {
                let source: Value = store::json_read(&path)?;
                let mut config = serde_json::json!({"hasCompletedOnboarding":true});
                config.as_object_mut().unwrap().extend(
                    crate::shared::account_config(&source)
                        .as_object()
                        .unwrap()
                        .clone(),
                );
                store::json_write(&target.join(".claude.json"), &config)?;
            }
        } else if provider == Provider::Codex {
            store::atomic(
                &target.join("config.toml"),
                b"cli_auth_credentials_store = \"file\"\n",
            )?;
        }
        let account = Account {
            provider,
            label: name.to_owned(),
        };
        state.accounts.push(account.clone());
        state
            .active
            .entry(provider)
            .or_insert_with(|| name.to_owned());
        store.save(&state)?;
        Ok(account)
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&target);
    }
    result
}
/// Activate a stored login. Never follow Home Manager symlinks or replace unreadable config.
pub fn switch(store: &Store, provider: Provider, name: &str, activate: bool) -> Result<Account> {
    // A token refresh holds the account's refresh lock and then needs the
    // store lock: while one runs, let go of the store lock and try again.
    let end = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        match switch_once(store, provider, name, activate) {
            Err(Error(crate::refresh::BUSY)) if std::time::Instant::now() < end => {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            result => return result,
        }
    }
}
fn switch_once(store: &Store, provider: Provider, name: &str, activate: bool) -> Result<Account> {
    let _lock = store.lock()?;
    let mut state = store.load()?;
    let account = store.account(&state, provider, Some(name))?;
    if activate {
        activate_profile(store, &account)?;
    }
    state.active.insert(provider, name.to_owned());
    store.save(&state)?;
    Ok(account)
}
/// How many live-login backups a provider keeps (oldest pruned).
const LIVE_BACKUPS: usize = 20;
/// A login's credential identity: a hash of its refresh token (else access
/// token), parsed, so formatting never makes one login look like another.
fn credential(provider: Provider, auth: &Value) -> Option<String> {
    let pointers: &[&str] = match provider {
        Provider::Claude => &["/claudeAiOauth/refreshToken", "/claudeAiOauth/accessToken"],
        Provider::Codex => &["/tokens/refresh_token", "/tokens/access_token"],
        Provider::Agy => &["/token/refresh_token", "/token/access_token"],
    };
    let token = pointers.iter().find_map(|p| {
        auth.pointer(p)
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
    })?;
    Some(
        ring::digest::digest(&ring::digest::SHA256, token.as_bytes())
            .as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
    )
}
/// What `--activate-live` last put in one live slot (its auth file): the
/// label and the credential. That slot then changes only by the provider's
/// own CLI rotating it, so a different credential there is that rotation.
#[derive(serde::Serialize, serde::Deserialize)]
struct Activation {
    /// The live auth file, canonical (the slot this record describes).
    slot: std::path::PathBuf,
    label: String,
    credential: String,
}
/// A live slot's identity: its auth file with the directory canonical.
fn slot(auth_path: &std::path::Path) -> std::path::PathBuf {
    let dir = auth_path.parent().unwrap_or(std::path::Path::new("."));
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_owned());
    dir.join(auth_path.file_name().unwrap_or_default())
}
fn activation_path(
    store: &Store,
    provider: Provider,
    slot: &std::path::Path,
) -> std::path::PathBuf {
    let hash: String =
        ring::digest::digest(&ring::digest::SHA256, slot.as_os_str().as_encoded_bytes())
            .as_ref()
            .iter()
            .take(16)
            .map(|b| format!("{b:02x}"))
            .collect();
    store
        .root
        .join("live-activations")
        .join(format!("{}-{hash}.json", provider.as_str()))
}
fn read_activation(
    store: &Store,
    provider: Provider,
    slot: &std::path::Path,
) -> Option<Activation> {
    let record: Activation = store::json_read(&activation_path(store, provider, slot)).ok()?;
    (record.slot == slot).then_some(record)
}
/// Beside a backup: which account's stored login it was compared with, and
/// that login's credential then (for `restore` to tell whether the profile
/// changed since).
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct BackupNote {
    label: Option<String>,
    profile_credential: Option<String>,
}
const BACKUP_NOTE: &str = "aswap-backup.json";
/// Write `bytes` (and Claude's config) into a new private backup directory.
fn back_up(
    store: &Store,
    provider: Provider,
    prefix: &str,
    bytes: &[u8],
    config: Option<&Value>,
    note: &BackupNote,
) -> Result<std::path::PathBuf> {
    let backups = store.root.join("live-backups").join(provider.as_str());
    store::private_dir(&backups)?;
    let backup = tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(&backups)
        .safe("cannot preserve outgoing login")?;
    store::atomic(&backup.path().join(provider.auth_file()), bytes)?;
    if let Some(config) = config {
        store::json_write(&backup.path().join(".claude.json"), config)?;
    }
    store::json_write(&backup.path().join(BACKUP_NOTE), note)?;
    let backup = backup.keep();
    prune_backups(&backups, &backup);
    Ok(backup)
}
/// A profile's login and config, and its authoritative account key.
fn stored(store: &Store, account: &Account) -> Result<(Value, Option<String>)> {
    let profile = store.profile(account.provider, &account.label)?;
    let saved: Value = store::json_read(&profile.join(account.provider.auth_file()))?;
    let config: Value =
        store::json_read(&profile.join(".claude.json")).unwrap_or_else(|_| serde_json::json!({}));
    let key = crate::identity::account_key(account.provider, &saved, &config);
    Ok((saved, key))
}
/// The outgoing login of one live slot (`auth_path`), before an activation
/// replaces it.
///
/// It is always backed up (live-backups, pruned to `LIVE_BACKUPS`). It is
/// saved, as a refresh would install it, only to the account aswap put live
/// in THIS slot (its activation record), when that account is the same
/// account (authoritative key), its profile still holds exactly what aswap
/// put live, the slot now holds another login, and no rotation is pending:
/// the provider's CLI rotated it, so it is that account's newest login.
/// Otherwise it is backed up only, and the user is told where.
fn preserve_live(
    store: &Store,
    target: &Account,
    bytes: &[u8],
    config: Option<&Value>,
    auth_path: &std::path::Path,
) -> Result<()> {
    validate_auth(target.provider, bytes)?;
    let provider = target.provider;
    let current: Value =
        serde_json::from_slice(bytes).safe("invalid live credential JSON; contents redacted")?;
    let empty = serde_json::json!({});
    let live_key = crate::identity::account_key(provider, &current, config.unwrap_or(&empty));
    let live_credential = credential(provider, &current);
    let accounts: Vec<Account> = store
        .load()?
        .accounts
        .into_iter()
        .filter(|a| a.provider == provider)
        .collect();
    // The same account's profiles (aliases included), by authoritative key.
    let mut same = Vec::new();
    if live_key.is_some() {
        for account in &accounts {
            let (saved, key) = stored(store, account)?;
            if key == live_key {
                same.push((account.clone(), saved));
            }
        }
    }
    let record = read_activation(store, provider, &slot(auth_path));
    let rotated = record.as_ref().and_then(|record| {
        let (account, saved) = same.iter().find(|(a, _)| a.label == record.label)?;
        (credential(provider, saved).as_ref() == Some(&record.credential)
            && live_credential.is_some()
            && live_credential.as_ref() != Some(&record.credential))
        .then_some(account.clone())
    });
    let compared = rotated
        .clone()
        .or_else(|| same.first().map(|(a, _)| a.clone()));
    let note = BackupNote {
        label: compared.as_ref().map(|a| a.label.clone()),
        profile_credential: compared.as_ref().and_then(|a| {
            same.iter()
                .find(|(b, _)| b.label == a.label)
                .and_then(|(_, saved)| credential(provider, saved))
        }),
    };
    let backup = back_up(store, provider, "before-switch-", bytes, config, &note)?;
    if let Some(account) = rotated {
        // Under the store lock: only try (lock order; switch retries).
        let _refresh = store
            .try_refresh_lock(&account)?
            .ok_or(Error(crate::refresh::BUSY))?;
        if !crate::refresh::pending_exists(store, &account) {
            return crate::refresh::install_login(store, &account, &current);
        }
    }
    let restore = format!(
        "aswap restore {provider} <label> --from {}",
        backup.display()
    );
    match &compared {
        Some(account) => eprintln!(
            "aswap: the live {provider} login of {} isn't provably newer than its stored login, so it was not saved; it is in {}. Restore it with: {restore}",
            account.label,
            backup.display()
        ),
        None => eprintln!(
            "aswap: the live {provider} login isn't a known account here; saved it to {}. Restore it with: {restore}",
            backup.display()
        ),
    }
    Ok(())
}
/// Keep the newest `LIVE_BACKUPS` backups (and `keep`).
fn prune_backups(dir: &std::path::Path, keep: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut backups: Vec<(std::time::SystemTime, std::path::PathBuf)> = entries
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("before-"))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    backups.sort();
    let excess = backups.len().saturating_sub(LIVE_BACKUPS);
    for (_, path) in backups.into_iter().take(excess) {
        if path != keep {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}
/// `aswap restore <provider> <label> --from <backup> [--force]`: install a
/// backed-up live login as that account's login, as a refresh would. It
/// must be the same account. Without `force` it refuses when a rotation is
/// pending, or when the profile's login changed since the backup was made
/// (it may be newer). The replaced login (and any pending copy) is backed
/// up first.
pub fn restore(
    store: &Store,
    provider: Provider,
    label: &str,
    from: &std::path::Path,
    force: bool,
) -> Result<(Account, std::path::PathBuf)> {
    let _lock = store.lock()?;
    let state = store.load()?;
    let account = store.account(&state, provider, Some(label))?;
    store::no_links(from)?;
    let bytes = store::read_private(&from.join(provider.auth_file()))?;
    validate_auth(provider, &bytes)?;
    let backup: Value =
        serde_json::from_slice(&bytes).safe("invalid backup JSON; contents redacted")?;
    let empty = serde_json::json!({});
    let backup_config: Value =
        store::json_read(&from.join(".claude.json")).unwrap_or(empty.clone());
    let (saved, saved_key) = stored(store, &account)?;
    let backup_key = crate::identity::account_key(provider, &backup, &backup_config);
    if backup_key.is_none() || backup_key != saved_key {
        return Err(Error(
            "the backup isn't a login of that account; use aswap add for a different account",
        ));
    }
    let _refresh = store
        .try_refresh_lock(&account)?
        .ok_or(Error(crate::refresh::BUSY))?;
    let pending = crate::refresh::pending_exists(store, &account);
    let note: BackupNote = store::json_read(&from.join(BACKUP_NOTE)).unwrap_or_default();
    let unchanged = note.profile_credential.is_some()
        && note.profile_credential == credential(provider, &saved);
    if !force && pending {
        return Err(Error(
            "that account has a pending rotated login (newer than the backup); --force replaces it (it is backed up first)",
        ));
    }
    if !force && !unchanged {
        return Err(Error(
            "that account's login changed since the backup and may be newer; --force replaces it (it is backed up first)",
        ));
    }
    // Back up what is replaced: the profile's login and any pending copy.
    let profile = store.profile(provider, label)?;
    let current = store::read_private(&profile.join(provider.auth_file()))?;
    let config: Option<Value> = store::json_read(&profile.join(".claude.json")).ok();
    let replaced = back_up(
        store,
        provider,
        "before-restore-",
        &current,
        config.as_ref(),
        &BackupNote {
            label: Some(label.to_owned()),
            profile_credential: credential(provider, &saved),
        },
    )?;
    for (n, copy) in crate::refresh::pending_paths(store, &account)
        .into_iter()
        .filter(|p| p.exists())
        .enumerate()
    {
        let bytes = store::read_private(&copy)?;
        store::atomic(&replaced.join(format!("pending-{n}.json")), &bytes)?;
    }
    crate::refresh::install_login(store, &account, &backup)?;
    Ok((account, replaced))
}
pub fn activate_profile(store: &Store, account: &Account) -> Result<()> {
    let provider = account.provider;
    let profile = store.profile(provider, &account.label)?;
    // A rotation of the incoming account saved only in its pending copy is
    // installed first, so activation never puts the revoked login live.
    {
        let _refresh = store
            .try_refresh_lock(account)?
            .ok_or(Error(crate::refresh::BUSY))?;
        crate::refresh::recover_pending(store, account, &profile.join(provider.auth_file()))?;
    }
    let bytes = store::read_private(&profile.join(provider.auth_file()))?;
    validate_auth(provider, &bytes)?;
    let dest = provider.current_home()?;
    let auth_path = dest.join(provider.auth_file());
    store::no_links(&auth_path)?;
    let old_auth = if auth_path.exists() {
        Some(store::read_private(&auth_path)?)
    } else {
        None
    };
    // Construct the merged config before writing either file.
    let mut old_config = None;
    let config = if provider == Provider::Claude {
        let path = global_config(&dest, true)?;
        let old: Option<Value> = if path.exists() {
            Some(store::json_read(&path)?)
        } else {
            None
        };
        old_config = old.clone();
        let mut merged = old.unwrap_or(serde_json::json!({}));
        if !merged.is_object() {
            return Err(Error("Claude config must be an object; refusing overwrite"));
        }
        let saved_path = profile.join(".claude.json");
        let saved: Value = if saved_path.exists() {
            store::json_read(&saved_path)?
        } else {
            serde_json::json!({})
        };
        if let Some(identity) = saved.get("oauthAccount") {
            merged["oauthAccount"] = identity.clone();
        } else {
            merged.as_object_mut().unwrap().remove("oauthAccount");
        }
        Some((path, merged))
    } else {
        None
    };
    if let Some(old) = &old_auth {
        preserve_live(store, account, old, old_config.as_ref(), &auth_path)?;
    }
    store::atomic(&auth_path, &bytes)?;
    if let Some((path, value)) = config
        && let Err(e) = store::json_write(&path, &value)
    {
        if let Some(old) = old_auth {
            store::atomic(&auth_path, &old)?;
        } else {
            std::fs::remove_file(&auth_path).safe("cannot roll back credential activation")?;
        }
        return Err(e);
    }
    // Last, once the activation stands: what this slot holds now, so the
    // next switch can tell the CLI's own rotation.
    let record = activation_path(store, provider, &slot(&auth_path));
    match serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(|auth| credential(provider, &auth))
    {
        Some(credential) => {
            if let Some(parent) = record.parent() {
                store::private_dir(parent)?;
            }
            store::json_write(
                &record,
                &Activation {
                    slot: slot(&auth_path),
                    label: account.label.clone(),
                    credential,
                },
            )?;
        }
        None => {
            let _ = std::fs::remove_file(&record);
        }
    }
    Ok(())
}
pub fn remove(store: &Store, provider: Provider, name: &str) -> Result<Vec<String>> {
    let _lock = store.lock()?;
    let mut state = store.load()?;
    let account = store.account(&state, provider, Some(name))?;
    let _lease = store.lease_label(&account)?;
    let profile = store.profile(provider, name)?;
    store::no_links(&profile)?;
    let duplicates = crate::identity::duplicates(store, &account, &state.accounts);
    // First unlink metadata; a failed deletion leaves an unselectable private orphan.
    state
        .accounts
        .retain(|a| !(a.provider == provider && a.label == name));
    if state.active.get(&provider).is_some_and(|s| s == name) {
        state.active.remove(&provider);
    }
    if let Some(settings) = state.auto.get_mut(&provider) {
        settings.reserved.retain(|label| label != name);
    }
    store.save(&state)?;
    // Including the copy under $XDG_RUNTIME_DIR, a refresh-capable login.
    crate::refresh::discard_pending(store, &account);
    if profile.exists() {
        std::fs::remove_dir_all(profile).safe("cannot remove private profile")?;
    }
    Ok(duplicates)
}
