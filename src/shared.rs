//! Shared provider state with private, disposable authentication views.
//! No provider data is logged. Development uses synthetic homes only.
use crate::{
    Error, Result, Safe,
    provider::{self, Provider},
    store::{self, Account, Store},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    fs::{self, File},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt, symlink},
    path::{Path, PathBuf},
};

// Based on the published Claude CLI's login/logout, identity and quota caches.
// Unknown account/auth-prefixed fields are private too; never copy default auth.
pub fn account_key(key: &str) -> bool {
    matches!(
        key,
        "oauthAccount"
            | "userID"
            | "anonymousId"
            | "primaryApiKey"
            | "customApiKeyResponses"
            | "hasAvailableSubscription"
            | "subscriptionNoticeCount"
            | "hasCompletedOnboarding"
            | "cachedExtraUsageDisabledReason"
            | "cachedStatsigGates"
            | "cachedDynamicConfigs"
            | "cachedGrowthBookFeatures"
    ) || [
        "oauth",
        "account",
        "organization",
        "subscription",
        "auth",
        "cachedOAuth",
        "cachedAccount",
        "cachedSubscription",
    ]
    .iter()
    .any(|prefix| key.starts_with(prefix))
}
pub fn account_config(config: &Value) -> Value {
    Value::Object(
        config
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(k, _)| account_key(k))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    )
}
fn non_account(config: &Value) -> Value {
    Value::Object(
        config
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(k, _)| !account_key(k))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    )
}
fn private(provider: Provider, name: &OsStr) -> bool {
    let name = name.to_string_lossy();
    name == provider.auth_file()
        || matches!(
            name.as_ref(),
            ".claude.json"
                | ".config.json"
                | "usage-cache.json"
                | ".aswap-shared-migrated"
                | "backups"
                | "remote-settings.json"
                | "policy-limits.json"
        )
        || name.starts_with(".credentials")
        || name.starts_with("auth")
        || name.starts_with(".auth")
        || name.starts_with(".claude.json.")
        || name.contains("oauth")
        || name.contains("token")
        || name.contains("credential")
}
fn entries(path: &Path) -> Result<Vec<PathBuf>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    fs::read_dir(path)
        .safe("cannot inspect provider state")?
        .map(|e| e.map(|e| e.path()).safe("cannot inspect provider state"))
        .collect()
}
fn link(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}
fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}
fn dir(path: &Path) -> Result<bool> {
    if store::no_links(path).is_err() {
        return Ok(false);
    }
    if exists(path) {
        return Ok(path.is_dir());
    }
    if let Some(parent) = path.parent()
        && !dir(parent)?
    {
        return Ok(false);
    }
    fs::DirBuilder::new()
        .mode(0o700)
        .create(path)
        .safe("cannot create shared provider directory")?;
    Ok(true)
}
/// Stream into a private temporary file, then publish without replacing anything.
fn new_file(source: &Path, dest: &Path) -> Result<()> {
    let parent = dest.parent().ok_or(Error("missing shared directory"))?;
    if exists(dest) || !dir(parent)? {
        return Ok(());
    }
    let mut input = File::open(source).safe("cannot read migration source")?;
    let mut temp = tempfile::NamedTempFile::new_in(parent).safe("cannot prepare shared file")?;
    temp.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))
        .safe("cannot secure shared file")?;
    std::io::copy(&mut input, &mut temp).safe("cannot copy shared file")?;
    temp.as_file().sync_all().safe("cannot sync shared file")?;
    match temp.persist_noclobber(dest) {
        Ok(_) => Ok(()),
        Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(_) => Err(Error("cannot publish shared file")),
    }
}
/// Migration never follows source/destination links and never overwrites a file.
pub fn merge_new(source: &Path, dest: &Path) -> Result<()> {
    if link(source) || store::no_links(dest).is_err() {
        return Ok(());
    }
    if source.is_dir() {
        if !dir(dest)? {
            return Ok(());
        }
        for child in entries(source)? {
            merge_new(&child, &dest.join(child.file_name().unwrap()))?;
        }
    } else if source.is_file() {
        new_file(source, dest)?;
    }
    Ok(())
}
// Managed entries are read-only inputs: snapshot their targets, never link them
// into the provider's writable namespace, and never merge edits back.
fn snapshot(source: &Path, dest: &Path, depth: usize) -> Result<()> {
    if depth > 40 {
        return Err(Error("managed provider entry exceeds nesting limit"));
    }
    if source.is_dir() {
        store::private_dir(dest)?;
        for child in entries(source)? {
            snapshot(&child, &dest.join(child.file_name().unwrap()), depth + 1)?;
        }
    } else if source.is_file() {
        new_file(source, dest)?;
    }
    Ok(())
}
fn read_config(path: &Path) -> Result<Value> {
    if !exists(path) {
        return Ok(json!({}));
    }
    let mut bytes = Vec::new();
    File::open(path)
        .safe("cannot read shared config")?
        .take(4 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .safe("cannot read shared config")?;
    if bytes.len() > 4 * 1024 * 1024 {
        return Err(Error("shared config exceeds size limit"));
    }
    let value: Value =
        serde_json::from_slice(&bytes).safe("invalid shared config; contents redacted")?;
    if !value.is_object() {
        return Err(Error("provider config must be an object"));
    }
    Ok(value)
}
/// Leaf-wise optimistic merge: preserve concurrent edits; unchanged keys do not
/// get written back. Account fields never enter this merge.
pub fn merge_changes(base: &Value, after: &Value, current: &mut Value) {
    if base == after {
        return;
    }
    if let (Some(base), Some(after), Some(current)) =
        (base.as_object(), after.as_object(), current.as_object_mut())
    {
        let keys: BTreeSet<_> = base.keys().chain(after.keys()).collect();
        for key in keys {
            match (base.get(key), after.get(key), current.get_mut(key)) {
                (Some(b), Some(a), Some(c)) => merge_changes(b, a, c),
                (None, Some(a), None) => {
                    current.insert(key.clone(), a.clone());
                }
                (Some(b), None, Some(c)) if b == c => {
                    current.remove(key);
                }
                _ => {}
            }
        }
    } else if current == base {
        *current = after.clone();
    }
}
fn merge_missing(source: &Value, dest: &mut Value) {
    if let (Some(source), Some(dest)) = (source.as_object(), dest.as_object_mut()) {
        for (k, v) in source {
            if let Some(old) = dest.get_mut(k) {
                merge_missing(v, old);
            } else {
                dest.insert(k.clone(), v.clone());
            }
        }
    }
}
#[derive(Clone, PartialEq)]
struct Stamp(u64, u64, u64, i64, i64);
fn stamp(path: &Path) -> Option<Stamp> {
    let m = fs::symlink_metadata(path).ok()?;
    m.is_file()
        .then(|| Stamp(m.dev(), m.ino(), m.len(), m.mtime(), m.mtime_nsec()))
}
fn global_config(provider: Provider, home: &Path) -> Result<PathBuf> {
    if provider != Provider::Claude {
        return Ok(home.join(".unused-config"));
    }
    let legacy = home.join(".config.json");
    if exists(&legacy) {
        return Ok(legacy);
    }
    if std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|s| !s.is_empty())
        .is_some()
    {
        Ok(home.join(".claude.json"))
    } else {
        Ok(provider::home()?.join(".claude.json"))
    }
}
fn migrate_root(
    store: &Store,
    provider: Provider,
    source: &Path,
    home: &Path,
    config_path: &Path,
) -> Result<()> {
    if !source.is_dir() || link(source) {
        return Ok(());
    }
    let config = source.join(".claude.json");
    if provider == Provider::Claude && config.exists() {
        let old: Value = store::json_read(&config)?;
        let mut shared = read_config(config_path)?;
        let before = shared.clone();
        merge_missing(&non_account(&old), &mut shared);
        if before != shared && store::no_links(config_path).is_ok() {
            store::json_write(config_path, &shared)?;
        }
        if old != account_config(&old) {
            let backup = store.root.join("migration-backups").join(provider.as_str());
            store::private_dir(&backup)?;
            let backup = tempfile::Builder::new()
                .prefix("config-")
                .tempdir_in(backup)
                .safe("cannot preserve isolated config")?
                .keep();
            store::json_write(&backup.join(".claude.json"), &old)?;
        }
        store::json_write(&config, &account_config(&old))?;
    }
    let candidates: Vec<_> = entries(source)?
        .into_iter()
        .filter(|p| !private(provider, p.file_name().unwrap()) && !link(p))
        .collect();
    if candidates.is_empty() {
        return Ok(());
    }
    let backup = store.root.join("migration-backups").join(provider.as_str());
    store::private_dir(&backup)?;
    let backup = tempfile::Builder::new()
        .prefix("isolated-")
        .tempdir_in(backup)
        .safe("cannot preserve isolated state")?
        .keep();
    for path in candidates {
        let name = path.file_name().unwrap();
        merge_new(&path, &home.join(name))?;
        fs::rename(&path, backup.join(name)).safe("cannot retain isolated migration source")?;
    }
    Ok(())
}
/// A private auth/config view; actual history lives in the caller's home.
pub struct View {
    pub path: PathBuf,
    pub home: PathBuf,
    provider: Provider,
    config_path: PathBuf,
    baseline: Value,
    managed: BTreeSet<OsString>,
    stamps: BTreeMap<OsString, Stamp>,
}
impl View {
    pub fn prepare(store: &Store, account: &Account) -> Result<Self> {
        let home = account.provider.current_home()?;
        if home.ancestors().any(|p| p.join(".git").exists()) {
            return Err(Error("provider state must live outside a Git checkout"));
        }
        let _lock = store.lock()?;
        if !dir(&home)? {
            return Err(Error("provider home must be an unmanaged directory"));
        }
        let config_path = global_config(account.provider, &home)?;
        let mut all_idle = true;
        let mut leases = Vec::new();
        for candidate in store
            .load()?
            .accounts
            .into_iter()
            .filter(|a| a.provider == account.provider)
        {
            if candidate.label != account.label {
                match store.lease_label(&candidate) {
                    Ok(lease) => leases.push(lease),
                    Err(_) => {
                        all_idle = false;
                        continue;
                    }
                }
            }
            let profile = store.profile(candidate.provider, &candidate.label)?;
            migrate_root(store, candidate.provider, &profile, &home, &config_path)?;
            for legacy in entries(&store.sessions(candidate.provider, &candidate.label)?)? {
                migrate_root(store, candidate.provider, &legacy, &home, &config_path)?;
            }
        }
        if all_idle {
            for legacy in entries(&store.root.join("sessions"))? {
                if legacy
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("run-"))
                    && legacy.join(account.provider.auth_file()).is_file()
                {
                    migrate_root(store, account.provider, &legacy, &home, &config_path)?;
                }
            }
        }
        let parent = store
            .root
            .join("runtimes")
            .join(account.provider.as_str())
            .join(&account.label);
        store::private_dir(&parent)?;
        let path = tempfile::Builder::new()
            .prefix("run-")
            .tempdir_in(parent)
            .safe("cannot create private launch view")?
            .keep();
        let mut names: BTreeSet<OsString> = entries(&home)?
            .into_iter()
            .map(|p| p.file_name().unwrap().to_owned())
            .collect();
        let dirs: &[&str] = if account.provider == Provider::Claude {
            &[
                "projects",
                "sessions",
                "todos",
                "shell-snapshots",
                "statsig",
                "plans",
                "file-history",
                "skills",
                "plugins",
                "agents",
                "commands",
                "tasks",
                "debug",
                "cache",
            ]
        } else {
            &[
                "sessions",
                "archived_sessions",
                "skills",
                "plugins",
                "rules",
                "memories",
                "tmp",
                "logs",
            ]
        };
        for name in dirs {
            names.insert((*name).into());
            if !exists(&home.join(name)) {
                dir(&home.join(name))?;
            }
        }
        let history = home.join("history.jsonl");
        if !exists(&history) {
            let file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&history)
                .safe("cannot create shared history")?;
            file.set_permissions(fs::Permissions::from_mode(0o600))
                .safe("cannot secure shared history")?;
        }
        names.insert("history.jsonl".into());
        let mut view = Self {
            path,
            home,
            provider: account.provider,
            config_path,
            baseline: json!({}),
            managed: BTreeSet::new(),
            stamps: BTreeMap::new(),
        };
        for name in names {
            if private(view.provider, &name) {
                continue;
            }
            let src = view.home.join(&name);
            let dst = view.path.join(&name);
            if link(&src) {
                snapshot(&src, &dst, 0)?;
                view.managed.insert(name);
            } else {
                if let Some(stamp) = stamp(&src) {
                    view.stamps.insert(name.clone(), stamp);
                }
                symlink(&src, &dst).safe("cannot link shared provider state")?;
            }
        }
        if view.provider == Provider::Claude {
            view.baseline = non_account(&read_config(&view.config_path)?);
        }
        view.select_locked(store, account)?;
        Ok(view)
    }
    fn select_locked(&self, store: &Store, account: &Account) -> Result<()> {
        let profile = store.profile(account.provider, &account.label)?;
        let auth = store::read_private(&profile.join(account.provider.auth_file()))?;
        provider::validate_auth(account.provider, &auth)?;
        let auth = if crate::coordination::follower(store)? {
            serde_json::to_vec(&crate::refresh::launch_auth(
                account.provider,
                &serde_json::from_slice(&auth).safe("invalid profile auth")?,
            ))
            .safe("cannot encode launch auth")?
        } else {
            auth
        };
        store::atomic(&self.path.join(account.provider.auth_file()), &auth)?;
        if account.provider == Provider::Claude {
            let mut config = if self.path.join(".claude.json").exists() {
                non_account(&store::json_read(&self.path.join(".claude.json"))?)
            } else {
                self.baseline.clone()
            };
            let own = profile.join(".claude.json");
            let own = if own.exists() {
                store::json_read(&own)?
            } else {
                json!({})
            };
            config
                .as_object_mut()
                .unwrap()
                .extend(account_config(&own).as_object().unwrap().clone());
            config
                .as_object_mut()
                .unwrap()
                .entry("hasCompletedOnboarding")
                .or_insert(Value::Bool(true));
            store::json_write(&self.path.join(".claude.json"), &config)?;
        }
        Ok(())
    }
    pub fn select(&self, store: &Store, account: &Account) -> Result<()> {
        let _lock = store.lock()?;
        self.select_locked(store, account)
    }
    pub fn sync(&mut self, store: &Store, account: &Account) -> Result<()> {
        let _lock = store.lock()?;
        let profile = store.profile(account.provider, &account.label)?;
        let auth = store::read_private(&self.path.join(account.provider.auth_file()))?;
        provider::validate_auth(account.provider, &auth)?;
        if crate::coordination::follower(store)? {
            let canonical: Value = store::json_read(&profile.join(account.provider.auth_file()))?;
            store::json_write(
                &self.path.join(account.provider.auth_file()),
                &crate::refresh::launch_auth(account.provider, &canonical),
            )?;
        } else {
            let previous: Value = store::json_read(&profile.join(account.provider.auth_file()))?;
            let current: Value = serde_json::from_slice(&auth).safe("invalid launch auth")?;
            let before_identity =
                crate::identity::read(account.provider, &profile).unwrap_or_default();
            store::atomic(&profile.join(account.provider.auth_file()), &auth)?;
            if previous != current {
                crate::usage::credentials_changed(store, account)?;

                let runtime_config = if account.provider == Provider::Claude {
                    store::json_read(&self.path.join(".claude.json"))?
                } else {
                    json!({})
                };
                let after =
                    crate::identity::from_values(account.provider, &current, &runtime_config);
                if crate::identity::same(&before_identity, &after) {
                    crate::refresh::propagate(store, account, &current)?;
                }
            }
        }
        if self.provider == Provider::Claude {
            let config: Value = store::json_read(&self.path.join(".claude.json"))?;
            if !crate::coordination::follower(store)? {
                store::json_write(&profile.join(".claude.json"), &account_config(&config))?;
            }
            let after = non_account(&config);
            let mut current = read_config(&self.config_path)?;
            let before = current.clone();
            let mut shared = non_account(&current);
            merge_changes(&self.baseline, &after, &mut shared);
            current
                .as_object_mut()
                .unwrap()
                .retain(|k, _| account_key(k));
            current
                .as_object_mut()
                .unwrap()
                .extend(shared.as_object().unwrap().clone());
            if current != before && store::no_links(&self.config_path).is_ok() {
                store::json_write(&self.config_path, &current)?;
            }
            self.baseline = after;
        }
        for path in entries(&self.path)? {
            let name = path.file_name().unwrap();
            if private(self.provider, name) || self.managed.contains(name) || link(&path) {
                continue;
            }
            let dest = self.home.join(name);
            if link(&dest) {
                continue;
            }
            // Providers may atomically replace a launch-view symlink. Only
            // publish ordinary configuration files when the source is unchanged.
            let configurable = matches!(
                name.to_str(),
                Some("settings.json" | "config.toml" | "CLAUDE.md" | "keybindings.json")
            );
            if configurable
                && path.is_file()
                && self
                    .stamps
                    .get(name)
                    .is_some_and(|before| stamp(&dest).as_ref() == Some(before))
            {
                let mut temp = tempfile::NamedTempFile::new_in(&self.home)
                    .safe("cannot prepare shared config")?;
                std::io::copy(
                    &mut File::open(&path).safe("cannot read launch config")?,
                    &mut temp,
                )
                .safe("cannot copy launch config")?;
                temp.flush().safe("cannot flush shared config")?;
                temp.as_file()
                    .sync_all()
                    .safe("cannot sync shared config")?;
                temp.persist(&dest).safe("cannot publish shared config")?;
                if let Some(stamp) = stamp(&dest) {
                    self.stamps.insert(name.to_owned(), stamp);
                }
            } else {
                merge_new(&path, &dest)?;
            }
        }
        Ok(())
    }
}
