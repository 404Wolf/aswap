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
    io::Write,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
};

mod config;
mod files;
mod handout;
mod mcp;

pub use config::{account_config, account_key, merge_changes};
use config::{merge_missing, read_config, shared_config};
pub use files::merge_new;
pub(crate) use files::{Stamp, stamp};
use files::{WAL_SIDECARS, dir, entries, exists, link, snapshot, sqlite_database};
mod sqlite;
pub(crate) use handout::hand_out;
use handout::{carry, view_lock};
use mcp::{MCP_OAUTH, mcp_record, with_mcp};
pub use sqlite::{SqliteRisk, ViewRisk, sqlite_risks};

/// A mkdir lock (proper-lockfile: a directory named `<file>.lock`), as Claude
/// Code takes for `.storage-write` and `mcp-refresh-*.lock`. One is never
/// linked into a view or published from one: through a symlink it can never
/// be released or broken as stale (rmdir of a link fails with ENOTDIR, and
/// mkdir over a dangling link fails with EEXIST), so one lock left in the
/// shared home would block that write in every view for good. It guards a
/// file in the same directory, and a view's own copy is what its processes
/// share. Lock *files* (flock on a regular `*.lock`, as Codex's
/// `session_index.jsonl.lock`) do work through a link and stay shared.
fn lock_dir(path: &Path) -> bool {
    path.file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| name.ends_with(".lock"))
        && fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
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
                // Claude's mkdir lock for writes to its auth file. The auth
                // file is per view, so its lock is too; through a symlink the
                // lock can never be removed and every MCP OAuth write fails.
                | ".storage-write.lock"
        )
        || name.starts_with(".credentials")
        || name.starts_with("auth")
        || name.starts_with(".auth")
        || name.starts_with(".claude.json.")
        || name.contains("oauth")
        || name.contains("token")
        || name.contains("credential")
}
fn global_config(provider: Provider, home: &Path) -> Result<PathBuf> {
    if provider.spec().account_config().is_none() {
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
    if provider.spec().account_config().is_some() && config.exists() {
        let old: Value = store::json_read(&config)?;
        let mut shared = read_config(config_path)?;
        let before = shared.clone();
        merge_missing(&shared_config(&old), &mut shared);
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
        .filter(|p| !private(provider, p.file_name().unwrap()) && !link(p) && !lock_dir(p))
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
/// Entries a launch view always links to the shared home of the same name.
fn shared_names(provider: Provider) -> impl Iterator<Item = &'static str> {
    let spec = provider.spec();
    spec.shared_dirs()
        .iter()
        .copied()
        .chain(spec.history_file())
}
/// A provider started inside an aswap run inherits CODEX_HOME or
/// CLAUDE_CONFIG_DIR pointing at that run's launch view. Treating the view
/// as home would snapshot every shared entry (each is a symlink there),
/// copying the whole session history into the new view. Follow the view's
/// links back to the home it shares instead.
fn shared_home(store: &Store, provider: Provider, mut home: PathBuf) -> Result<PathBuf> {
    let runtimes = store.root.join("runtimes");
    for _ in 0..8 {
        if !home.starts_with(&runtimes) {
            return Ok(home);
        }
        let target = shared_names(provider).find_map(|name| {
            let target = fs::read_link(home.join(name)).ok()?;
            (target.is_absolute() && target.file_name() == Some(OsStr::new(name)))
                .then(|| target.parent().map(Path::to_path_buf))
                .flatten()
        });
        // Views copied by earlier releases have no links left; their real
        // home is the provider's default one.
        home = match target {
            Some(target) => target,
            None => provider::home()?.join(provider.spec().home_dir()),
        };
    }
    Err(Error("provider home is a chain of aswap launch views"))
}
/// Removes a run's launch view, with its lock and session report, when the
/// run ends, but only once `synced` says its last sync published everything
/// it holds of the user's: what remains is disposable (an access-only login,
/// account config already merged back, symlinks). A view whose last sync
/// failed is kept, so nothing written there is lost.
pub struct ViewCleanup {
    path: PathBuf,
    home: PathBuf,
    views: PathBuf,
    provider: Provider,
    pub synced: bool,
}
impl ViewCleanup {
    pub fn new(store: &Store, view: &View) -> Self {
        Self {
            path: view.path.clone(),
            home: view.home.clone(),
            views: views_root(store, view.provider),
            provider: view.provider,
            synced: false,
        }
    }
}
impl Drop for ViewCleanup {
    fn drop(&mut self) {
        if !self.synced {
            return;
        }
        // The provider's index must not keep naming files through the view.
        let scope = crate::provider::index::Scope::View(&self.path);
        if let Err(err) = self
            .provider
            .spec()
            .repair_index(&self.home, &self.views, scope)
        {
            eprintln!("aswap: {err}; keeping the launch view so its transcripts stay reachable");
            return;
        }
        // remove_dir_all never follows the view's links into shared state.
        let _ = fs::remove_dir_all(&self.path);
        for suffix in [".lock", ".session.json"] {
            let mut name = self.path.as_os_str().to_owned();
            name.push(suffix);
            let _ = fs::remove_file(PathBuf::from(name));
        }
    }
}
/// Where `provider`'s launch views live: `<store>/runtimes/<provider>`.
pub fn views_root(store: &Store, provider: Provider) -> PathBuf {
    store.root.join("runtimes").join(provider.as_str())
}
/// The home a launch view of `provider` shares, as `View::prepare` finds it.
pub fn shared_home_of(store: &Store, provider: Provider) -> Result<PathBuf> {
    shared_home(store, provider, provider.current_home()?)
}
/// A private auth/config view; actual history lives in the caller's home.
pub struct View {
    pub path: PathBuf,
    pub home: PathBuf,
    provider: Provider,
    config_path: PathBuf,
    baseline: Value,
    account_baseline: Value,
    /// The MCP record this view last received; native edits are diffed
    /// against it.
    mcp_baseline: Value,
    managed: BTreeSet<OsString>,
    stamps: BTreeMap<OsString, Stamp>,
    /// The canonical profile auth this view last copied; a different stamp
    /// means the owner rotated or a follower installed a new token.
    auth_stamp: Option<Stamp>,
}
impl View {
    /// The global store lock covers migration, shared-home creation and the
    /// credential/MCP writes in `select_locked`. Linking and snapshotting into
    /// the new run directory, which no other process knows about, happen
    /// outside it: a large managed entry must never stall other launches.
    pub fn prepare(store: &Store, account: &Account) -> Result<Self> {
        let home = shared_home(store, account.provider, account.provider.current_home()?)?;
        if home.ancestors().any(|p| p.join(".git").exists()) {
            return Err(Error("provider state must live outside a Git checkout"));
        }
        let lock = store.lock()?;
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
            if candidate.label == account.label {
                // Managed peers only write shared state, never these legacy
                // directories. The first prepare may migrate them even if several
                // new sessions registered before reaching this store lock.
                all_idle &= store.live_count(&candidate)? <= 1;
                match store.legacy_lock(&candidate) {
                    Ok(guard) => leases.push(guard),
                    Err(_) => {
                        all_idle = false;
                        continue;
                    }
                }
            } else {
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
        let parent = views_root(store, account.provider).join(&account.label);
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
        let spec = account.provider.spec();
        for name in spec.shared_dirs() {
            names.insert((*name).into());
            if !exists(&home.join(name)) {
                dir(&home.join(name))?;
            }
        }
        if let Some(name) = spec.history_file() {
            let history = home.join(name);
            if !exists(&history) {
                let file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&history)
                    .safe("cannot create shared history")?;
                file.set_permissions(fs::Permissions::from_mode(0o600))
                    .safe("cannot secure shared history")?;
            }
            names.insert(name.into());
        }
        // SQLite opens a linked database's -wal and -shm beside the real
        // file, so Codex's own SQLite never uses the view's spelling. A client
        // that did would create them privately, beside a shared database:
        // two WALs for one database. Link them before they exist, so every
        // spelling reaches the home's.
        let databases: Vec<OsString> = names
            .iter()
            .filter(|name| sqlite_database(&home, name))
            .cloned()
            .collect();
        for database in databases {
            for suffix in WAL_SIDECARS {
                let mut sidecar = database.clone();
                sidecar.push(suffix);
                names.insert(sidecar);
            }
        }
        drop(lock);
        let mut view = Self {
            path,
            home,
            provider: account.provider,
            config_path,
            baseline: json!({}),
            account_baseline: json!({}),
            mcp_baseline: json!({}),
            managed: BTreeSet::new(),
            stamps: BTreeMap::new(),
            auth_stamp: None,
        };
        for name in names {
            if private(view.provider, &name) || lock_dir(&view.home.join(&name)) {
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
        let _lock = store.lock()?;
        if view.provider.spec().account_config().is_some() {
            view.baseline = shared_config(&read_config(&view.config_path)?);
        }
        view.select_locked(store, account)?;
        drop(_lock);
        // Index entries left through views that earlier runs never cleaned
        // up (or an older aswap removed): best effort, never a launch failure.
        let views = views_root(store, view.provider);
        let scope = crate::provider::index::Scope::Gone;
        let _ = view.provider.spec().repair_index(&view.home, &views, scope);
        Ok(view)
    }
    fn select_locked(&mut self, store: &Store, account: &Account) -> Result<()> {
        let profile = store.profile(account.provider, &account.label)?;
        let observed = stamp(&profile.join(account.provider.auth_file()));
        let auth = store::read_private(&profile.join(account.provider.auth_file()))?;
        provider::validate_auth(account.provider, &auth)?;
        let mut auth = crate::refresh::launch_auth(
            account.provider,
            &serde_json::from_slice(&auth).safe("invalid profile auth")?,
        );
        let auth_path = self.path.join(account.provider.auth_file());
        // Switch the view and its account label together: a hand-out for the
        // previous account that arrives after this sees the new label.
        let lock = view_lock(&self.path, true)?;
        if account.provider.spec().mcp_oauth() {
            let seen = store::json_read::<Value>(&auth_path).ok();
            auth = with_mcp(auth, &self.merge_mcp(store, seen.as_ref())?);
        }
        let auth = serde_json::to_vec(&auth).safe("cannot encode launch auth")?;
        store::atomic(&auth_path, &auth)?;
        carry(&lock, account)?;
        drop(lock);
        self.auth_stamp = observed;
        if account.provider.spec().account_config().is_some() {
            let mut config = if self.path.join(".claude.json").exists() {
                shared_config(&store::json_read(&self.path.join(".claude.json"))?)
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
            self.account_baseline = account_config(&config);
            store::json_write(&self.path.join(".claude.json"), &config)?;
        }
        Ok(())
    }
    /// Merge this view's native MCP OAuth edits into the user's record and
    /// return the record the view should carry now. An unreadable or MCP-less
    /// view (mid-write, or not launched yet) deletes nothing.
    fn merge_mcp(&mut self, store: &Store, seen: Option<&Value>) -> Result<Value> {
        let mut record = mcp_record(store)?;
        if let Some(native) = seen.and_then(|auth| auth.get(MCP_OAUTH)) {
            let before = record.clone();
            merge_changes(&self.mcp_baseline, native, &mut record);
            if record != before {
                store::json_write(&store.mcp_auth(Provider::Claude), &record)?;
            }
        }
        self.mcp_baseline = record.clone();
        Ok(record)
    }
    /// Whether the account's canonical auth changed since this view last
    /// copied it. One stat and no store lock, so a run can ask every tick and
    /// hand a rotated token to its provider within a tick instead of 30s.
    pub fn auth_changed(&self, store: &Store, account: &Account) -> bool {
        store
            .profile(account.provider, &account.label)
            .is_ok_and(|p| stamp(&p.join(account.provider.auth_file())) != self.auth_stamp)
    }
    /// Where the provider reports its current session: beside the view, never
    /// inside the provider home, like the view's lock.
    pub fn report_path(&self) -> PathBuf {
        let mut name = self.path.as_os_str().to_owned();
        name.push(".session.json");
        PathBuf::from(name)
    }
    pub fn select(&mut self, store: &Store, account: &Account) -> Result<()> {
        let _lock = store.lock()?;
        self.select_locked(store, account)
    }
    pub fn sync(&mut self, store: &Store, account: &Account) -> Result<()> {
        let _lock = store.lock()?;
        let profile = store.profile(account.provider, &account.label)?;
        // Read the profile and write the view under the view's lock, so a
        // hand-out cannot land in between and be rolled back to the old token.
        let held = view_lock(&self.path, true)?;
        // Launch views never own refresh tokens. Re-read the authority rather than
        // letting a stale/late native write roll canonical credentials backwards.
        // Stamp before reading: a write after the read shows up as a change.
        let observed = stamp(&profile.join(account.provider.auth_file()));
        let canonical: Value = store::json_read(&profile.join(account.provider.auth_file()))?;
        let auth_path = self.path.join(account.provider.auth_file());
        let seen = store::json_read::<Value>(&auth_path).ok();
        let mut auth = crate::refresh::launch_auth(account.provider, &canonical);
        if self.provider.spec().mcp_oauth() {
            auth = with_mcp(auth, &self.merge_mcp(store, seen.as_ref())?);
        }
        // Rewrite only on change, and never over a native write that landed
        // after the read above: that edit merges on the next pass instead.
        if seen.as_ref() != Some(&auth) {
            if store::json_read::<Value>(&auth_path).ok() == seen {
                store::json_write(&auth_path, &auth)?;
                self.auth_stamp = observed;
            } else if let Some(native) = seen.as_ref().and_then(|s| s.get(MCP_OAUTH)) {
                // Leave the stamp: the next tick retries the canonical copy.
                self.mcp_baseline = native.clone();
            }
        } else {
            self.auth_stamp = observed;
        }
        drop(held);
        if self.provider.spec().account_config().is_some() {
            let config: Value = store::json_read(&self.path.join(".claude.json"))?;
            if !crate::coordination::follower(store)? {
                let after = account_config(&config);
                let own = profile.join(".claude.json");
                let mut current = if own.exists() {
                    store::json_read(&own)?
                } else {
                    json!({})
                };
                merge_changes(&self.account_baseline, &after, &mut current);
                store::json_write(&own, &current)?;
                self.account_baseline = after;
            }
            let after = shared_config(&config);
            let mut current = read_config(&self.config_path)?;
            let before = current.clone();
            // Only shared keys enter the diff; home-owned account and process
            // state stays untouched, even when the run adds or removes it.
            merge_changes(&self.baseline, &after, &mut current);
            if current != before && store::no_links(&self.config_path).is_ok() {
                store::json_write(&self.config_path, &current)?;
            }
            self.baseline = after;
        }
        for path in entries(&self.path)? {
            let name = path.file_name().unwrap();
            if private(self.provider, name)
                || self.managed.contains(name)
                || link(&path)
                || lock_dir(&path)
            {
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

#[cfg(test)]
mod lock_tests {
    use super::*;
    #[test]
    fn auth_file_write_lock_stays_in_its_view() {
        assert!(private(Provider::Claude, OsStr::new(".storage-write.lock")));
    }
}
