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
    os::unix::fs::{DirBuilderExt, FileExt, MetadataExt, OpenOptionsExt, PermissionsExt, symlink},
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
// Claude's fullscreen boot canary tracks process liveness. Copying it between
// launches makes stale pids look like failed boots after an account switch.
const PER_PROCESS_KEYS: &[&str] = &[
    "fullscreenBootPending",
    "fullscreenBootStrikes",
    "fullscreenAutoDisabled",
];
fn shared_config(config: &Value) -> Value {
    Value::Object(
        config
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(k, _)| !account_key(k) && !PER_PROCESS_KEYS.contains(&k.as_str()))
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
/// SQLite sidecars are transient and belong to the connection's database path.
fn sqlite_sidecar(path: &Path) -> bool {
    path.file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| {
            ["-wal", "-shm", "-journal"].iter().any(|suffix| {
                name.strip_suffix(suffix).is_some_and(|base| {
                    base.ends_with(".sqlite") || base.ends_with(".sqlite3") || base.ends_with(".db")
                })
            })
        })
}
fn sqlite_file(path: &Path) -> bool {
    let mut header = [0; 16];
    File::open(path)
        .and_then(|mut f| f.read_exact(&mut header))
        .is_ok()
        && &header == b"SQLite format 3\0"
}
/// Copy committed pages through SQLite so live WAL writes cannot tear snapshots.
fn sqlite_snapshot(source: &Path, dest: &Path) -> Result<()> {
    let source =
        rusqlite::Connection::open_with_flags(source, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .safe("cannot open SQLite snapshot source")?;
    source
        .busy_timeout(std::time::Duration::from_secs(5))
        .safe("cannot set SQLite timeout")?;
    let mut target =
        rusqlite::Connection::open(dest).safe("cannot open SQLite snapshot destination")?;
    let backup = rusqlite::backup::Backup::new(&source, &mut target)
        .safe("cannot prepare SQLite snapshot")?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if matches!(
            backup.step(128).safe("cannot snapshot SQLite database")?,
            rusqlite::backup::StepResult::Done
        ) {
            break;
        }
        if std::time::Instant::now() >= deadline {
            return Err(Error("SQLite snapshot timed out; original retained"));
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    drop(backup);
    let check: String = target
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .safe("cannot validate SQLite snapshot")?;
    if check != "ok" {
        return Err(Error(
            "SQLite snapshot source is corrupt; original retained",
        ));
    }
    Ok(())
}
/// Stream into a private temporary file, then publish without replacing anything.
fn new_file(source: &Path, dest: &Path) -> Result<()> {
    let parent = dest.parent().ok_or(Error("missing shared directory"))?;
    if exists(dest) || !dir(parent)? {
        return Ok(());
    }
    if sqlite_sidecar(source) {
        return Ok(());
    }
    let mut temp = tempfile::NamedTempFile::new_in(parent).safe("cannot prepare shared file")?;
    temp.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))
        .safe("cannot secure shared file")?;
    if sqlite_file(source) {
        sqlite_snapshot(source, temp.path())?;
    } else {
        let mut input = File::open(source).safe("cannot read migration source")?;
        std::io::copy(&mut input, &mut temp).safe("cannot copy shared file")?;
    }
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
/// Claude keeps MCP server OAuth (client registrations, access and refresh
/// tokens) beside the account login in .credentials.json. It belongs to the
/// user, not to the account: one store-wide record feeds every launch view and
/// survives account switches, and native logins, registrations and rotations
/// in a view merge back into it. aswap never refreshes MCP tokens, so views
/// keep their MCP refresh tokens exactly as a native Claude home would.
const MCP_OAUTH: &str = "mcpOAuth";
fn mcp_record(store: &Store) -> Result<Value> {
    let path = store.mcp_auth(Provider::Claude);
    if path.exists() {
        return store::json_read(&path);
    }
    // First use: adopt what earlier releases left in the profiles. Those
    // copies lost their refresh tokens; the latest expiry per server wins.
    let expiry = |entry: &Value| entry.get("expiresAt").and_then(Value::as_u64);
    let mut seed = serde_json::Map::new();
    for account in store.load()?.accounts {
        if account.provider != Provider::Claude {
            continue;
        }
        let profile = store.profile(account.provider, &account.label)?;
        let Ok(auth) = store::json_read::<Value>(&profile.join(account.provider.auth_file()))
        else {
            continue;
        };
        if let Some(servers) = auth.get(MCP_OAUTH).and_then(Value::as_object) {
            for (server, entry) in servers {
                if seed
                    .get(server)
                    .is_none_or(|old| expiry(entry) > expiry(old))
                {
                    seed.insert(server.clone(), entry.clone());
                }
            }
        }
    }
    let seed = Value::Object(seed);
    store::json_write(&path, &seed)?;
    Ok(seed)
}
fn with_mcp(mut auth: Value, record: &Value) -> Value {
    if let Some(auth) = auth.as_object_mut() {
        if record.as_object().is_some_and(|r| !r.is_empty()) {
            auth.insert(MCP_OAUTH.into(), record.clone());
        } else {
            auth.remove(MCP_OAUTH);
        }
    }
    auth
}
/// A file's identity and version: atomic replacement changes the inode.
#[derive(Clone, PartialEq)]
pub(crate) struct Stamp(u64, u64, u64, i64, i64);
pub(crate) fn stamp(path: &Path) -> Option<Stamp> {
    let m = fs::symlink_metadata(path).ok()?;
    m.is_file()
        .then(|| Stamp(m.dev(), m.ino(), m.len(), m.mtime(), m.mtime_nsec()))
}
/// A launch view's own lock file, beside the view and never inside the
/// provider home. It holds the label of the account whose credentials the view
/// carries, so a hand-out never gives one account's token to a view that has
/// switched to another. Closing the file releases the lock.
fn view_lock(view: &Path, create: bool) -> Result<File> {
    let mut name = view.as_os_str().to_owned();
    name.push(".lock");
    let path = PathBuf::from(name);
    store::no_links(&path)?;
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .safe("cannot open launch view lock")?;
    fs2::FileExt::lock_exclusive(&file).safe("cannot lock launch view")?;
    Ok(file)
}
fn carry(lock: &File, account: &Account) -> Result<()> {
    lock.set_len(0).safe("cannot record launch view account")?;
    lock.write_all_at(account.label.as_bytes(), 0)
        .safe("cannot record launch view account")
}
fn carries(mut lock: &File, account: &Account) -> bool {
    let mut label = String::new();
    lock.read_to_string(&mut label).is_ok() && label == account.label
}
/// Replace a view's auth file by rename, so its provider reads the old file or
/// the new one and never a partial write. Views are disposable, so there is
/// no fsync. The temporary name is one `private` skips: a run syncing its view
/// at the same moment never copies it into the shared home.
fn replace_view_auth(path: &Path, bytes: &[u8]) -> Result<()> {
    store::no_links(path)?;
    let parent = path.parent().ok_or(Error("missing launch view"))?;
    let name = path
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or(Error("missing launch view"))?;
    let mut temp = tempfile::Builder::new()
        .prefix(&format!("{name}.handout-"))
        .permissions(fs::Permissions::from_mode(0o600))
        .tempfile_in(parent)
        .safe("cannot create private temporary file")?;
    temp.write_all(bytes).safe("cannot write private file")?;
    temp.persist(path).safe("cannot replace private file")?;
    Ok(())
}
/// Hand a rotated account token to every live run's view in one pass, right
/// after its profile changes. The provider revokes the old access token at
/// once, so this cannot wait for each run's tick, and it takes no store lock:
/// each view is replaced by rename under that view's own lock. A view that
/// switched to another account, or a run from an older release without a
/// view lock, is left to its run's own sync. Returns the views updated.
pub(crate) fn hand_out(store: &Store, account: &Account, auth: &Value) -> usize {
    let fresh = crate::refresh::launch_auth(account.provider, auth);
    let runtimes = store.root.join("runtimes");
    crate::live::views(store, account)
        .into_iter()
        .filter(|view| {
            view.starts_with(&runtimes) && hand_to(view, account, &fresh).is_ok_and(|done| done)
        })
        .count()
}
fn hand_to(view: &Path, account: &Account, fresh: &Value) -> Result<bool> {
    let lock = view_lock(view, false)?;
    if !carries(&lock, account) {
        return Ok(false);
    }
    let path = view.join(account.provider.auth_file());
    let seen: Value = store::json_read(&path)?;
    let mut next = fresh.clone();
    if account.provider == Provider::Claude
        && let Some(next) = next.as_object_mut()
    {
        // MCP OAuth in the view is the user's; the run's own sync merges it.
        match seen.get(MCP_OAUTH) {
            Some(mcp) => next.insert(MCP_OAUTH.into(), mcp.clone()),
            None => next.remove(MCP_OAUTH),
        };
    }
    if seen == next {
        return Ok(false);
    }
    replace_view_auth(
        &path,
        &serde_json::to_vec(&next).safe("cannot encode launch auth")?,
    )?;
    Ok(true)
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
/// Entries a launch view always links to the shared home of the same name.
fn shared_names(provider: Provider) -> &'static [&'static str] {
    if provider == Provider::Claude {
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
    }
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
        let names = shared_names(provider)
            .iter()
            .copied()
            .chain(["history.jsonl"]);
        let target = names.into_iter().find_map(|name| {
            let target = fs::read_link(home.join(name)).ok()?;
            (target.is_absolute() && target.file_name() == Some(OsStr::new(name)))
                .then(|| target.parent().map(Path::to_path_buf))
                .flatten()
        });
        // Views copied by earlier releases have no links left; their real
        // home is the provider's default one.
        home = match target {
            Some(target) => target,
            None => provider::home()?.join(match provider {
                Provider::Claude => ".claude",
                Provider::Codex => ".codex",
            }),
        };
    }
    Err(Error("provider home is a chain of aswap launch views"))
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
        let dirs = shared_names(account.provider);
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
        let _lock = store.lock()?;
        if view.provider == Provider::Claude {
            view.baseline = shared_config(&read_config(&view.config_path)?);
        }
        view.select_locked(store, account)?;
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
        if account.provider == Provider::Claude {
            let seen = store::json_read::<Value>(&auth_path).ok();
            auth = with_mcp(auth, &self.merge_mcp(store, seen.as_ref())?);
        }
        let auth = serde_json::to_vec(&auth).safe("cannot encode launch auth")?;
        store::atomic(&auth_path, &auth)?;
        carry(&lock, account)?;
        drop(lock);
        self.auth_stamp = observed;
        if account.provider == Provider::Claude {
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
        if self.provider == Provider::Claude {
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
        if self.provider == Provider::Claude {
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

#[cfg(test)]
mod config_tests {
    use super::*;
    #[test]
    fn snapshot_omits_only_account_and_top_level_process_keys() {
        let config = json!({
            "oauthAccount": {"accountUuid": "synthetic"},
            "fullscreenBootPending": {"123": {"startedAt": 1}},
            "fullscreenBootStrikes": {"count": 2},
            "fullscreenAutoDisabled": {"strikes": 2},
            "fullscreenUpsellSeenCount": 3,
            "theme": "dark",
            "projects": {"synthetic": {"fullscreenBootPending": true}}
        });
        assert_eq!(
            shared_config(&config),
            json!({
                "fullscreenUpsellSeenCount": 3,
                "theme": "dark",
                "projects": {"synthetic": {"fullscreenBootPending": true}}
            })
        );
    }
    #[test]
    fn process_changes_preserve_home_state_while_other_keys_merge() {
        let base = shared_config(&json!({"theme": "dark"}));
        let after = shared_config(&json!({
            "theme": "light",
            "fullscreenBootPending": {"456": {"startedAt": 2}},
            "fullscreenBootStrikes": {"count": 3},
            "fullscreenAutoDisabled": {"strikes": 3}
        }));
        for mut current in [
            json!({"theme": "dark"}),
            json!({
                "theme": "dark",
                "fullscreenBootPending": {"123": {"startedAt": 1}},
                "fullscreenBootStrikes": {"count": 1},
                "fullscreenAutoDisabled": {"strikes": 2}
            }),
        ] {
            let mut expected = current.clone();
            expected["theme"] = json!("light");
            merge_changes(&base, &after, &mut current);
            assert_eq!(current, expected);
        }
    }
}

#[cfg(test)]
mod sqlite_tests {
    use super::*;
    #[test]
    fn live_wal_snapshots_include_committed_rows_and_omit_sidecars() {
        let t = tempfile::tempdir().unwrap();
        let source = t.path().join("source");
        fs::create_dir(&source).unwrap();
        let db = source.join("thread_history_1.sqlite");
        let writer = rusqlite::Connection::open(&db).unwrap();
        writer.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE items (id INTEGER); INSERT INTO items VALUES (42);").unwrap();
        assert!(source.join("thread_history_1.sqlite-wal").exists());
        for managed in [false, true] {
            let dest = t
                .path()
                .join(if managed { "snapshot" } else { "migration" });
            if managed {
                snapshot(&source, &dest, 0).unwrap();
            } else {
                merge_new(&source, &dest).unwrap();
            }
            assert!(!dest.join("thread_history_1.sqlite-wal").exists());
            assert!(!dest.join("thread_history_1.sqlite-shm").exists());
            let reader = rusqlite::Connection::open(dest.join("thread_history_1.sqlite")).unwrap();
            assert_eq!(
                reader
                    .query_row("SELECT id FROM items", [], |r| r.get::<_, i64>(0))
                    .unwrap(),
                42
            );
            assert_eq!(
                reader
                    .query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
                    .unwrap(),
                "ok"
            );
        }
    }
    #[test]
    fn failed_sqlite_snapshot_is_not_published_and_original_is_retained() {
        let t = tempfile::tempdir().unwrap();
        let source = t.path().join("broken.sqlite");
        fs::write(
            &source,
            [b"SQLite format 3\0".as_slice(), &[0u8; 200]].concat(),
        )
        .unwrap();
        let dest = t.path().join("new.sqlite");
        assert!(new_file(&source, &dest).is_err());
        assert!(!dest.exists());
        assert!(source.exists());
    }
}
