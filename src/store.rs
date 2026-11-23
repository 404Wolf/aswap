use crate::provider::Provider;
use crate::{Error, Result, Safe};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Account {
    pub provider: Provider,
    pub label: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Auto {
    #[serde(default = "default_resume_prompt")]
    pub resume_prompt: Option<String>,
    pub enabled: bool,
    pub threshold: f64,
    pub last_switch: u64,
    #[serde(default)]
    pub prefer: Prefer,
    #[serde(default)]
    pub reserved: Vec<String>,
    /// Opt-in redemption of Codex usage-limit reset credits; see `crate::reset`.
    #[serde(default)]
    pub auto_reset: AutoReset,
    /// Settings a newer aswap wrote, kept as they are so this one doesn't
    /// drop them when it saves (docs/COMPATIBILITY.md: data directory).
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}
/// Whether the monitor may spend a Codex usage-limit reset credit.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum AutoReset {
    /// Never reset.
    #[default]
    Off,
    /// Log the reset aswap would make, without making it.
    DryRun,
    /// Redeem one reset credit when every Codex account is out of weekly usage.
    On,
}
impl AutoReset {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::DryRun => "dry-run",
            Self::On => "on",
        }
    }
}
/// How auto selection ranks eligible accounts.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Prefer {
    /// The lowest used percentage.
    Headroom,
    /// The soonest weekly reset, so quota that would expire unused goes
    /// first. The default when state does not set a preference.
    #[default]
    WeeklyReset,
}
pub const RESUME_PROMPT: &str =
    "You were interrupted by a usage-limit account switch. Continue exactly where you left off.";
pub fn default_resume_prompt() -> Option<String> {
    Some(RESUME_PROMPT.into())
}
impl Default for Auto {
    fn default() -> Self {
        Self {
            resume_prompt: default_resume_prompt(),
            enabled: false,
            threshold: 90.0,
            last_switch: 0,
            prefer: Prefer::WeeklyReset,
            reserved: Vec::new(),
            auto_reset: AutoReset::Off,
            extra: Default::default(),
        }
    }
}
impl Auto {
    /// These settings for --json output: without the fields only a newer
    /// aswap knows (kept in `extra` for state.json, not part of the output).
    pub fn public_json(&self) -> serde_json::Value {
        let mut value = serde_json::to_value(self).unwrap_or_default();
        if let Some(object) = value.as_object_mut() {
            for key in self.extra.keys() {
                object.remove(key);
            }
        }
        value
    }
    pub fn allows(&self, label: &str, pinned: Option<&str>) -> bool {
        pinned == Some(label) || !self.reserved.iter().any(|reserved| reserved == label)
    }
}
/// state.json. Everything this version doesn't know is kept and written
/// back unchanged: top-level fields, Auto fields, and the accounts, active
/// labels and auto settings of providers it doesn't know. So a running
/// older supervisor never drops what a newer aswap added, and an account of
/// a newer provider never stops this version from reading the file.
#[derive(Clone, Serialize, Deserialize)]
#[serde(try_from = "RawState", into = "RawState")]
pub struct State {
    pub accounts: Vec<Account>,
    pub active: BTreeMap<Provider, String>,
    pub auto: BTreeMap<Provider, Auto>,
    /// Machine-wide minimum spacing of supervisor account switches, across
    /// every provider (see `switch_gate`); 0 turns the gate off.
    pub switch_interval_seconds: u64,
    /// What a newer aswap wrote that this one doesn't know.
    pub(crate) foreign: Foreign,
}
#[derive(Clone, Default)]
pub(crate) struct Foreign {
    accounts: Vec<serde_json::Value>,
    active: serde_json::Map<String, serde_json::Value>,
    auto: serde_json::Map<String, serde_json::Value>,
    fields: serde_json::Map<String, serde_json::Value>,
}
/// state.json as written, before providers are recognized.
#[derive(Serialize, Deserialize)]
struct RawState {
    #[serde(default)]
    accounts: Vec<serde_json::Value>,
    #[serde(default)]
    active: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    auto: serde_json::Map<String, serde_json::Value>,
    #[serde(default = "default_switch_interval")]
    switch_interval_seconds: u64,
    #[serde(flatten)]
    fields: serde_json::Map<String, serde_json::Value>,
}
/// A provider name this version knows, or None.
fn known_provider(name: &str) -> Option<Provider> {
    serde_json::from_value(serde_json::Value::String(name.to_owned())).ok()
}
impl TryFrom<RawState> for State {
    type Error = serde_json::Error;
    fn try_from(raw: RawState) -> std::result::Result<Self, Self::Error> {
        use serde::de::Error as _;
        let mut state = Self {
            accounts: Vec::new(),
            active: BTreeMap::new(),
            auto: BTreeMap::new(),
            switch_interval_seconds: raw.switch_interval_seconds,
            foreign: Foreign {
                fields: raw.fields,
                ..Foreign::default()
            },
        };
        for account in raw.accounts {
            let provider = account.get("provider").and_then(serde_json::Value::as_str);
            match provider {
                Some(name) if known_provider(name).is_none() => {
                    state.foreign.accounts.push(account)
                }
                _ => state.accounts.push(serde_json::from_value(account)?),
            }
        }
        for (name, label) in raw.active {
            match known_provider(&name) {
                Some(provider) => {
                    let label = label
                        .as_str()
                        .ok_or_else(|| serde_json::Error::custom("active label is not a string"))?;
                    state.active.insert(provider, label.to_owned());
                }
                None => {
                    state.foreign.active.insert(name, label);
                }
            }
        }
        for (name, auto) in raw.auto {
            match known_provider(&name) {
                Some(provider) => {
                    state.auto.insert(provider, serde_json::from_value(auto)?);
                }
                None => {
                    state.foreign.auto.insert(name, auto);
                }
            }
        }
        Ok(state)
    }
}
impl From<State> for RawState {
    fn from(state: State) -> Self {
        let mut accounts: Vec<serde_json::Value> = state
            .accounts
            .iter()
            .filter_map(|a| serde_json::to_value(a).ok())
            .collect();
        accounts.extend(state.foreign.accounts);
        let mut active = state.foreign.active;
        for (provider, label) in state.active {
            active.insert(provider.as_str().to_owned(), label.into());
        }
        let mut auto = state.foreign.auto;
        for (provider, settings) in state.auto {
            if let Ok(value) = serde_json::to_value(settings) {
                auto.insert(provider.as_str().to_owned(), value);
            }
        }
        Self {
            accounts,
            active,
            auto,
            switch_interval_seconds: state.switch_interval_seconds,
            fields: state.foreign.fields,
        }
    }
}
fn default_switch_interval() -> u64 {
    crate::switch_gate::DEFAULT_INTERVAL_SECONDS
}
impl Default for State {
    fn default() -> Self {
        Self {
            accounts: Vec::new(),
            active: BTreeMap::new(),
            auto: BTreeMap::new(),
            switch_interval_seconds: default_switch_interval(),
            foreign: Foreign::default(),
        }
    }
}
/// The longest accepted `switch_interval_seconds`.
pub const MAX_SWITCH_INTERVAL: u64 = 3600;
#[derive(Clone)]
pub struct Store {
    pub root: PathBuf,
    pub(crate) sync_policy_path: PathBuf,
    scope: StoreScope,
}
#[derive(Clone)]
enum StoreScope {
    Standalone,
    Legacy,
    Agy {
        private_root: PathBuf,
        program: Option<PathBuf>,
    },
}

const LEGACY_MARKER: &str = "legacy-compatibility";
const LEGACY_MARKER_V1: &[u8] = b"aswap legacy compatibility v1\n";

fn legacy_marker(root: &Path) -> Result<bool> {
    let path = root.join(LEGACY_MARKER);
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Ok(metadata) if metadata.is_file() && metadata.len() == LEGACY_MARKER_V1.len() as u64 => {
            if read_private(&path)? == LEGACY_MARKER_V1 {
                Ok(true)
            } else {
                Err(Error("invalid legacy compatibility marker"))
            }
        }
        _ => Err(Error("invalid legacy compatibility marker")),
    }
}

/// Routes providers to their configured stores before any account data is read.
/// With no override, the original single-store layout remains available.
pub struct StoreView {
    legacy: PathBuf,
    agy: Option<AgyStore>,
}

#[derive(Default, Deserialize)]
struct Config {
    #[serde(default)]
    stores: ProviderStores,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderStores {
    agy: Option<AgyConfig>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgyConfig {
    root: Option<PathBuf>,
    private_root: Option<PathBuf>,
    program: Option<PathBuf>,
}

struct AgyStore {
    root: PathBuf,
    private_root: PathBuf,
    program: Option<PathBuf>,
}

fn store_config() -> Result<Config> {
    // Home Manager may install the declarative config as a symlink.
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(Store::config_dir()?.join("config.toml"))
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(_) => return Err(Error("cannot read aswap configuration")),
    };
    if !file
        .metadata()
        .safe("cannot inspect aswap configuration")?
        .is_file()
    {
        return Err(Error("aswap configuration must be a regular TOML file"));
    }
    const MAX_CONFIG: u64 = 1024 * 1024;
    let mut text = String::new();
    file.take(MAX_CONFIG + 1)
        .read_to_string(&mut text)
        .safe("cannot read aswap configuration")?;
    if text.len() as u64 > MAX_CONFIG {
        return Err(Error("aswap configuration exceeds size limit"));
    }
    toml::from_str(&text).safe("invalid aswap store configuration")
}

impl StoreView {
    pub fn from_env() -> Result<Self> {
        let legacy = Store::root_from_env()?;
        let configured = store_config()?.stores.agy.unwrap_or_default();
        let private = std::env::var_os("ASWAP_AGY_PRIVATE_ROOT")
            .map(PathBuf::from)
            .or(configured.private_root);
        let agy = match std::env::var_os("ASWAP_AGY_HOME")
            .map(PathBuf::from)
            .or(configured.root)
        {
            Some(root) => {
                if let Some(program) = &configured.program {
                    absolute_path(program)?;
                }
                let private = match private {
                    Some(path) => path,
                    None => crate::provider::home()?.join(".local/state/agy-private"),
                };
                // Component comparison also handles trailing slashes and ".".
                if root.starts_with(&legacy) || legacy.starts_with(&root) {
                    return Err(Error("agy and legacy account stores must be separate"));
                }
                if private.starts_with(&root)
                    || root.starts_with(&private)
                    || private.starts_with(&legacy)
                    || legacy.starts_with(&private)
                {
                    return Err(Error(
                        "agy private state and account stores must be separate",
                    ));
                }
                Some(AgyStore {
                    root,
                    private_root: private,
                    program: configured.program,
                })
            }
            None if private.is_some() || configured.program.is_some() => {
                return Err(Error(
                    "agy private_root and program require a separate agy store root",
                ));
            }
            None => None,
        };
        Ok(Self { legacy, agy })
    }

    pub fn legacy(&self) -> Result<Store> {
        let scope = if self.agy.is_some() {
            StoreScope::Legacy
        } else {
            StoreScope::Standalone
        };
        let store = Store::open_scoped(self.legacy.clone(), scope)?;
        if store.legacy_compatibility() {
            // First admission excludes an unfenced refresh until the marker
            // is durable. Do not wait for this lock while holding store.lock.
            let _admission = if legacy_marker(&store.root)? {
                None
            } else {
                Some(store.try_admission_lock(true)?.ok_or(Error(
                    "legacy store admission is busy; retry after credential refresh or concurrent admission finishes",
                ))?)
            };
            let _lock = store.lock()?;
            crate::refresh::require_legacy_pending_free(&store)?;
            if !legacy_marker(&store.root)? {
                // Validate the state before making the opt-in durable.
                store.load()?;
                atomic(&store.root.join(LEGACY_MARKER), LEGACY_MARKER_V1)?;
            }
        }
        Ok(store)
    }

    pub fn for_provider(&self, provider: Provider) -> Result<Store> {
        if provider == Provider::Agy
            && let Some(agy) = &self.agy
        {
            let store = Self::open_agy(agy)?;
            // Even an Agy-only invocation enables the fence for later
            // invocations that share the legacy root with another binary.
            self.legacy()?;
            return Ok(store);
        }
        let store = self.legacy()?;
        store.check_provider(provider)?;
        Ok(store)
    }

    pub fn stores(&self, filter: Option<Provider>) -> Result<Vec<Store>> {
        if let Some(provider) = filter {
            return Ok(vec![self.for_provider(provider)?]);
        }
        let mut stores = vec![self.legacy()?];
        if let Some(agy) = &self.agy {
            stores.push(Self::open_agy(agy)?);
        }
        Ok(stores)
    }

    fn open_agy(agy: &AgyStore) -> Result<Store> {
        Store::open_scoped(
            agy.root.clone(),
            StoreScope::Agy {
                private_root: agy.private_root.clone(),
                program: agy.program.clone(),
            },
        )
    }
}
/// A held file lock; `.1` marks the store lock, counted in `HELD`.
pub struct Guard(File, bool);
pub use crate::live::Lease;
impl Drop for Guard {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
        if self.1 {
            HELD.with(|held| held.set(held.get().saturating_sub(1)));
        }
    }
}
thread_local! {
    /// How many store locks this thread holds. Every supervisor takes the
    /// store lock on its terminal thread, so while it is held nothing may
    /// wait long: retries under it stop after `LOCKED_PATIENCE`, and callers
    /// that can afford to wait retry the whole locked step instead.
    static HELD: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}
/// Whether this thread holds the store lock.
pub(crate) fn lock_held() -> bool {
    HELD.with(|held| held.get()) > 0
}
const LOCKED_PATIENCE: std::time::Duration = std::time::Duration::from_millis(1500);

pub fn label(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 48
        || !value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        return Err(Error(
            "labels must be 1–48 ASCII letters, digits, hyphens or underscores",
        ));
    }
    Ok(())
}
fn absolute_path(path: &Path) -> Result<()> {
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(Error(
            "an absolute path without parent traversal is required",
        ));
    }
    Ok(())
}
/// Reject symlinks at every existing path component, including ancestors.
pub fn no_links(path: &Path) -> Result<()> {
    absolute_path(path)?;
    let mut p = PathBuf::new();
    for part in path.components() {
        p.push(part);
        match fs::symlink_metadata(&p) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(Error("refusing a symlink or managed file"));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(Error("cannot inspect private path")),
        }
    }
    Ok(())
}
pub fn private_dir(path: &Path) -> Result<()> {
    no_links(path)?;
    if !path.exists() {
        if let Some(parent) = path.parent()
            && !parent.exists()
        {
            private_dir(parent)?;
        }
        fs::DirBuilder::new()
            .mode(0o700)
            .create(path)
            .safe("cannot create private directory")?;
    }
    if !path.is_dir() {
        return Err(Error("private directory is not a directory"));
    }
    secure(path, 0o700, "cannot secure directory")
}
/// Set `mode` only when it differs: a chmod is a metadata write, which a full
/// filesystem (ZFS, btrfs) can refuse with ENOSPC even though nothing needs
/// changing. On 2026-11-14 that ended every new supervisor and the monitor.
fn secure(path: &Path, mode: u32, message: &'static str) -> Result<()> {
    let current = fs::metadata(path).safe(message)?.permissions().mode() & 0o7777;
    if current == mode {
        return Ok(());
    }
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).safe(message)
}
/// How long store I/O (locks, private writes) keeps retrying a failure such as
/// ENOSPC, EIO or EAGAIN, in milliseconds. A supervisor raises it while it
/// prepares a launch (nothing runs yet, so waiting beats exiting) and lowers
/// it while a provider runs (its bookkeeping retries on the next tick).
static PATIENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(5_000);
pub fn set_patience(patience: std::time::Duration) {
    PATIENCE.store(
        patience.as_millis().try_into().unwrap_or(u64::MAX),
        std::sync::atomic::Ordering::Relaxed,
    );
}
fn patience() -> std::time::Duration {
    std::time::Duration::from_millis(PATIENCE.load(std::sync::atomic::Ordering::Relaxed))
}
/// Whether an error is an I/O failure that may pass (a full disk, EIO,
/// EAGAIN, a busy lock): this crate words every one "cannot ...", while
/// refusals (a symlink, corrupt JSON, a missing account) are worded otherwise
/// and never pass by waiting.
pub fn transient(err: &Error) -> bool {
    err.0.starts_with("cannot ")
}
/// When store trouble was last reported (seconds since the epoch): a
/// supervisor's terminal shows the provider's UI, so an outage is reported
/// at most once a minute however many operations it fails.
static REPORTED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
fn report_due() -> bool {
    let at = crate::now();
    let last = REPORTED.load(std::sync::atomic::Ordering::Relaxed);
    at.saturating_sub(last) >= 60
        && REPORTED
            .compare_exchange(
                last,
                at,
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
            )
            .is_ok()
}
/// Run `op` until it succeeds, fails with a non-transient error, or
/// `patience` is spent, backing off from 50ms up to 5s between attempts.
/// Failures are reported on stderr (the monitor's journal, or the
/// supervisor's terminal), at most once a minute.
pub(crate) fn retry<T>(what: &str, op: impl FnMut() -> Result<T>) -> Result<T> {
    let patience = if HELD.with(|held| held.get()) > 0 {
        patience().min(LOCKED_PATIENCE)
    } else {
        patience()
    };
    retry_for(what, patience, std::time::Duration::from_millis(50), op)
}
pub(crate) fn retry_for<T>(
    what: &str,
    patience: std::time::Duration,
    first: std::time::Duration,
    mut op: impl FnMut() -> Result<T>,
) -> Result<T> {
    let start = std::time::Instant::now();
    let mut delay = first;
    let mut reported = false;
    loop {
        match op() {
            Ok(value) => {
                if reported {
                    eprintln!("aswap: {what} succeeded after retrying");
                }
                return Ok(value);
            }
            Err(err) if !transient(&err) => return Err(err),
            Err(err) => {
                let left = patience.saturating_sub(start.elapsed());
                if left.is_zero() {
                    if reported {
                        eprintln!("aswap: {what} still failing ({err}); giving up");
                    }
                    return Err(err);
                }
                if !reported && report_due() {
                    eprintln!(
                        "aswap: {what} failed ({err}); retrying for up to {}s",
                        left.as_secs().max(1)
                    );
                    reported = true;
                }
                std::thread::sleep(delay.min(left));
                delay = (delay * 2).min(std::time::Duration::from_secs(5));
            }
        }
    }
}
pub(crate) const PRIVATE_FILE_TOO_LARGE: &str = "private file exceeds size limit";

pub fn read_private(path: &Path) -> Result<Vec<u8>> {
    no_links(path)?;
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .safe("cannot read private file")?;
    if !file
        .metadata()
        .safe("cannot inspect private file")?
        .is_file()
    {
        return Err(Error("expected a regular private file"));
    }
    let mut bytes = Vec::new();
    std::io::Read::by_ref(&mut file)
        .take(4 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .safe("cannot read private file")?;
    if bytes.len() > 4 * 1024 * 1024 {
        return Err(Error(PRIVATE_FILE_TOO_LARGE));
    }
    Ok(bytes)
}
pub fn atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    no_links(path)?;
    let parent = path.parent().ok_or(Error("missing private parent"))?;
    retry("writing private state", || write_once(path, parent, bytes))
}
fn write_once(path: &Path, parent: &Path, bytes: &[u8]) -> Result<()> {
    private_dir(parent)?;
    let mut temp =
        tempfile::NamedTempFile::new_in(parent).safe("cannot create private temporary file")?;
    temp.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))
        .safe("cannot secure private file")?;
    temp.write_all(bytes).safe("cannot write private file")?;
    temp.as_file().sync_all().safe("cannot sync private file")?;
    temp.persist(path).safe("cannot replace private file")?;
    File::open(parent)
        .and_then(|f| f.sync_all())
        .safe("cannot sync private directory")
}
pub fn json_read<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    serde_json::from_slice(&read_private(path)?).safe("invalid private JSON; contents redacted")
}
pub fn json_write<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    atomic(
        path,
        &serde_json::to_vec_pretty(value).safe("cannot encode private state")?,
    )
}
impl Store {
    pub(crate) fn root_from_env() -> Result<PathBuf> {
        let base = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or(crate::provider::home()?.join(".local/share"));
        Ok(base.join("aswap"))
    }
    pub(crate) fn config_dir() -> Result<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or(crate::provider::home()?.join(".config"));
        Ok(base.join("aswap"))
    }
    pub fn from_env() -> Result<Self> {
        StoreView::from_env()?.legacy()
    }
    pub fn open(root: PathBuf) -> Result<Self> {
        Self::open_scoped(root, StoreScope::Standalone)
    }
    fn open_scoped(root: PathBuf, scope: StoreScope) -> Result<Self> {
        no_links(&root)?;
        // Public checkout must never become an account store, even by accident.
        if root.ancestors().any(|p| p.join(".git").exists()) {
            return Err(Error("account stores must live outside a Git checkout"));
        }
        let scope = if legacy_marker(&root)? {
            if matches!(scope, StoreScope::Agy { .. }) {
                return Err(Error("a fenced legacy store cannot be used for agy"));
            }
            StoreScope::Legacy
        } else {
            scope
        };
        let sync_policy_path = if let StoreScope::Agy { private_root, .. } = &scope {
            no_links(private_root)?;
            if private_root
                .ancestors()
                .any(|path| path.join(".git").exists())
            {
                return Err(Error("agy private state must live outside a Git checkout"));
            }
            private_root.join("config/aswap/sync-policy.json")
        } else {
            Self::config_dir()?.join("sync-policy.json")
        };
        private_dir(&root)?;
        Ok(Self {
            root,
            sync_policy_path,
            scope,
        })
    }

    pub fn agy_private_root(&self) -> Option<&Path> {
        match &self.scope {
            StoreScope::Agy { private_root, .. } => Some(private_root),
            _ => None,
        }
    }

    pub fn agy_program(&self) -> Option<&Path> {
        match &self.scope {
            StoreScope::Agy { program, .. } => program.as_deref(),
            _ => None,
        }
    }

    /// This root is shared with Claude/Codex binaries predating agy support.
    pub fn legacy_compatibility(&self) -> bool {
        matches!(self.scope, StoreScope::Legacy)
            // A standalone handle can outlive admission of this root.
            || !matches!(legacy_marker(&self.root), Ok(false))
    }

    pub fn provider_home(&self, provider: Provider) -> Result<PathBuf> {
        self.check_provider(provider)?;
        if let Some(root) = self.agy_private_root() {
            let home = root.join("home/.gemini");
            no_links(&home)?;
            Ok(home)
        } else {
            provider.current_home()
        }
    }

    /// Set agy's inherited directories while keeping XDG_CONFIG_HOME on
    /// the per-launch view selected by Provider::command.
    pub fn apply_agy_environment(&self, command: &mut Command) -> Result<()> {
        if let Some(root) = self.agy_private_root() {
            // Reuse the data home installed by the standalone wrapper when
            // its parent is private. A custom store may have a shared parent.
            use std::os::unix::fs::MetadataExt;
            // SAFETY: geteuid takes no arguments.
            let uid = unsafe { libc::geteuid() };
            let data = self.root.parent().filter(|p| {
                self.root.file_name().is_some_and(|name| name == "aswap")
                    && fs::metadata(p).is_ok_and(|m| m.uid() == uid && m.mode() & 0o7777 == 0o700)
            });
            let data = match data {
                Some(path) => path.to_owned(),
                None => {
                    let path = root.join("data");
                    private_dir(&path)?;
                    path
                }
            };
            for (key, path) in [
                ("HOME", root.join("home")),
                ("XDG_CACHE_HOME", root.join("cache")),
                ("XDG_STATE_HOME", root.join("state")),
                ("XDG_RUNTIME_DIR", root.join("runtime")),
            ] {
                private_dir(&path)?;
                command.env(key, path);
            }
            command.env("XDG_DATA_HOME", data);
        }
        Ok(())
    }

    pub fn command(&self, provider: Provider, profile: &Path) -> Result<Command> {
        self.check_provider(provider)?;
        let mut command = provider.command_with_bin(profile, self.agy_program());
        if provider == Provider::Agy {
            crate::agy::check_view_settings(profile)?;
            self.apply_agy_environment(&mut command)?;
        }
        Ok(command)
    }

    fn check_provider(&self, provider: Provider) -> Result<()> {
        if provider == Provider::Agy && self.legacy_compatibility() {
            return Err(Error(
                "agy requires a separate configured store on this fenced legacy root",
            ));
        }
        match (&self.scope, provider) {
            (StoreScope::Agy { .. }, Provider::Claude | Provider::Codex) => {
                Err(Error("account provider is assigned to a different store"))
            }
            _ => Ok(()),
        }
    }

    fn check_state(&self, state: &State) -> Result<()> {
        for provider in state
            .accounts
            .iter()
            .map(|a| a.provider)
            .chain(state.active.keys().copied())
            .chain(state.auto.keys().copied())
        {
            self.check_provider(provider)?;
        }
        if self.legacy_compatibility()
            && (!state.foreign.accounts.is_empty()
                || !state.foreign.active.is_empty()
                || !state.foreign.auto.is_empty())
        {
            return Err(Error(
                "legacy account store contains an unsupported provider",
            ));
        }
        Ok(())
    }
    pub fn lock(&self) -> Result<Guard> {
        let path = self.root.join("store.lock");
        no_links(&path)?;
        let guard = retry("locking the account store", || {
            let f = OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&path)
                .safe("cannot open store lock")?;
            // As for directories, only change a mode that differs; through
            // the descriptor, never the path.
            let mode = f
                .metadata()
                .safe("cannot secure lock")?
                .permissions()
                .mode()
                & 0o7777;
            if mode != 0o600 {
                f.set_permissions(fs::Permissions::from_mode(0o600))
                    .safe("cannot secure lock")?;
            }
            f.lock_exclusive().safe("cannot lock account store")?;
            HELD.with(|held| held.set(held.get() + 1));
            Ok(Guard(f, true))
        })?;
        crate::rename::recover(self)?;
        Ok(guard)
    }
    /// Refresh holds a shared lock through OAuth and persistence. First
    /// admission needs the exclusive lock before taking store.lock.
    pub(crate) fn try_admission_lock(&self, exclusive: bool) -> Result<Option<Guard>> {
        let path = self.root.join("legacy-admission.lock");
        no_links(&path)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
            .safe("cannot open legacy admission lock")?;
        if !file
            .metadata()
            .safe("cannot inspect legacy admission lock")?
            .is_file()
        {
            return Err(Error("legacy admission lock must be a regular file"));
        }
        let locked = if exclusive {
            FileExt::try_lock_exclusive(&file)
        } else {
            FileExt::try_lock_shared(&file)
        };
        match locked {
            Ok(()) => Ok(Some(Guard(file, false))),
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(_) => Err(Error("cannot lock legacy store admission")),
        }
    }
    pub fn load(&self) -> Result<State> {
        let p = self.root.join("state.json");
        let state: State = if p.exists() {
            json_read(&p)?
        } else {
            State::default()
        };
        self.check_state(&state)?;
        for a in &state.accounts {
            label(&a.label)?;
        }
        for l in state.active.values() {
            label(l)?;
        }
        for a in state.auto.values() {
            threshold(a.threshold)?;
            for reserved in &a.reserved {
                label(reserved)?;
            }
        }
        Ok(state)
    }
    pub fn save(&self, state: &State) -> Result<()> {
        self.check_state(state)?;
        json_write(&self.root.join("state.json"), state)
    }
    pub fn profile(&self, provider: Provider, name: &str) -> Result<PathBuf> {
        self.check_provider(provider)?;
        label(name)?;
        Ok(self
            .root
            .join("profiles")
            .join(provider.as_str())
            .join(name))
    }
    /// The user's MCP server OAuth record for a provider. It belongs to the
    /// person, not to any account, so it survives account switches.
    pub fn mcp_auth(&self, provider: Provider) -> PathBuf {
        self.root
            .join("mcp")
            .join(format!("{}.json", provider.as_str()))
    }
    pub fn sessions(&self, provider: Provider, name: &str) -> Result<PathBuf> {
        self.check_provider(provider)?;
        label(name)?;
        Ok(self
            .root
            .join("sessions")
            .join(provider.as_str())
            .join(name))
    }
    pub fn account(
        &self,
        state: &State,
        provider: Provider,
        name: Option<&str>,
    ) -> Result<Account> {
        let name = name
            .or_else(|| state.active.get(&provider).map(String::as_str))
            .ok_or(Error("no active account; add or switch first"))?;
        state
            .accounts
            .iter()
            .find(|a| a.provider == provider && a.label == name)
            .cloned()
            .ok_or(Error("account not found"))
    }
    pub fn lease(&self, account: &Account) -> Result<Lease> {
        let _lock = self.lock()?;
        self.lease_locked(account)
    }
    pub(crate) fn lease_locked(&self, account: &Account) -> Result<Lease> {
        crate::live::register(self, account)
    }
    pub(crate) fn live_count(&self, account: &Account) -> Result<usize> {
        crate::live::count(self, account)
    }
    /// Short identity critical section; caller also holds the store lock.
    /// `refresh_lock` without waiting: None while another holds it. Every
    /// holder of the store lock takes the refresh lock only this way (the
    /// lock order is refresh lock, then store lock), and on None drops the
    /// store lock and tries again.
    pub(crate) fn try_refresh_lock(&self, account: &Account) -> Result<Option<Guard>> {
        let (f, _) = self.refresh_lock_file(account)?;
        Ok(f.try_lock_exclusive().is_ok().then_some(Guard(f, false)))
    }
    fn refresh_lock_file(&self, account: &Account) -> Result<(File, ())> {
        let identity = crate::identity::read(
            account.provider,
            &self.profile(account.provider, &account.label)?,
        )
        .unwrap_or_default();
        let key = identity.lock_key().unwrap_or_else(|| account.label.clone());
        let path = self
            .root
            .join(format!("{}-{key}.refresh.lock", account.provider));
        no_links(&path)?;
        let f = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .safe("cannot open refresh lock")?;
        Ok((f, ()))
    }
    pub(crate) fn refresh_lock(&self, account: &Account) -> Result<Guard> {
        let identity = crate::identity::read(
            account.provider,
            &self.profile(account.provider, &account.label)?,
        )
        .unwrap_or_default();
        let key = identity.lock_key().unwrap_or_else(|| account.label.clone());
        let path = self
            .root
            .join(format!("{}-{key}.refresh.lock", account.provider));
        no_links(&path)?;
        let f = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .safe("cannot open refresh lock")?;
        f.lock_exclusive().safe("cannot lock credential refresh")?;
        Ok(Guard(f, false))
    }
    pub(crate) fn lease_label(&self, account: &Account) -> Result<Guard> {
        if self.live_count(account)? > 0 {
            return Err(Error(
                "account profile is in use; retry after its sessions exit",
            ));
        }
        self.legacy_lock(account)
    }
    /// The one process per follower store that keeps the owner's push stream
    /// open. None while another process (a run or the monitor) holds it.
    pub(crate) fn watch_lock(&self) -> Result<Option<Guard>> {
        let path = self.root.join("sync-watch.lock");
        no_links(&path)?;
        let f = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .safe("cannot open sync watch lock")?;
        Ok(f.try_lock_exclusive().is_ok().then_some(Guard(f, false)))
    }
    /// One usage fetch per login at a time, taken without the store lock
    /// so no supervisor waits on the network. None while another process
    /// fetches; `wait` bounds how long to keep trying.
    pub(crate) fn usage_lock(
        &self,
        account: &Account,
        wait: std::time::Duration,
    ) -> Result<Option<Guard>> {
        // Keyed like the refresh lock: aliases of one login share one quota,
        // so they share one fetch.
        let identity = crate::identity::read(
            account.provider,
            &self.profile(account.provider, &account.label)?,
        )
        .unwrap_or_default();
        let key = identity.lock_key().unwrap_or_else(|| account.label.clone());
        let path = self
            .root
            .join(format!("{}-{key}.usage.lock", account.provider));
        no_links(&path)?;
        let f = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .safe("cannot open usage lock")?;
        let start = std::time::Instant::now();
        loop {
            if f.try_lock_exclusive().is_ok() {
                return Ok(Some(Guard(f, false)));
            }
            if start.elapsed() >= wait {
                return Ok(None);
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    pub(crate) fn legacy_lock(&self, account: &Account) -> Result<Guard> {
        let path = self
            .root
            .join(format!("{}-{}.run.lock", account.provider, account.label));
        no_links(&path)?;
        let f = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .safe("cannot open profile lock")?;
        f.try_lock_exclusive()
            .safe("legacy refresh-capable session is still running")?;
        Ok(Guard(f, false))
    }
}
pub fn threshold(value: f64) -> Result<()> {
    if !value.is_finite() || !(1.0..=100.0).contains(&value) {
        return Err(Error("threshold must be between 1 and 100"));
    }
    Ok(())
}

#[cfg(test)]
mod retry_tests {
    use super::*;
    use std::{os::unix::fs::MetadataExt, time::Duration};

    #[test]
    fn a_lock_that_fails_a_few_times_is_retried_until_it_works() {
        let mut failures = 3;
        let mut calls = 0;
        let got = retry_for(
            "locking",
            Duration::from_secs(5),
            Duration::from_millis(1),
            || {
                calls += 1;
                if failures > 0 {
                    failures -= 1;
                    Err(Error("cannot lock account store"))
                } else {
                    Ok(7)
                }
            },
        );
        assert_eq!(got.unwrap(), 7);
        assert_eq!(calls, 4);
    }

    #[test]
    fn retrying_is_bounded_and_refusals_are_not_retried() {
        let started = std::time::Instant::now();
        let mut calls = 0;
        let got: Result<()> = retry_for(
            "writing",
            Duration::from_millis(60),
            Duration::from_millis(5),
            || {
                calls += 1;
                Err(Error("cannot write private file"))
            },
        );
        assert_eq!(got.unwrap_err().0, "cannot write private file");
        assert!(calls > 1 && started.elapsed() < Duration::from_secs(2));
        let mut calls = 0;
        let got: Result<()> = retry_for(
            "reading",
            Duration::from_secs(5),
            Duration::from_millis(1),
            || {
                calls += 1;
                Err(Error("refusing a symlink or managed file"))
            },
        );
        assert!(got.is_err());
        assert_eq!(calls, 1);
    }

    #[test]
    fn a_store_whose_lock_is_briefly_unusable_still_locks() {
        let t = tempfile::tempdir().unwrap();
        let store = Store::open(t.path().join("store")).unwrap();
        // Something occupies the lock's path (a stand-in for ENOSPC/EIO on
        // open): the lock waits for it to clear instead of failing.
        let lock = store.root.join("store.lock");
        fs::create_dir(&lock).unwrap();
        let clear = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            fs::remove_dir(&lock).unwrap();
        });
        drop(store.lock().unwrap());
        clear.join().unwrap();
    }

    #[test]
    fn a_failing_write_under_the_store_lock_does_not_hold_up_other_sessions() {
        let t = tempfile::tempdir().unwrap();
        let store = Store::open(t.path().join("store")).unwrap();
        // A write that keeps failing (a directory where the file goes, a
        // stand-in for ENOSPC), made while holding the store lock.
        let stuck = store.root.join("stuck.json");
        fs::create_dir(&stuck).unwrap();
        let guard = store.lock().unwrap();
        let root = store.root.clone();
        let other = std::thread::spawn(move || {
            let store = Store::open(root).unwrap();
            let waiting = std::time::Instant::now();
            drop(store.lock().unwrap());
            waiting.elapsed()
        });
        let started = std::time::Instant::now();
        assert!(atomic(&stuck, b"synthetic").is_err());
        // Well under the 5s it would retry without the lock held.
        assert!(started.elapsed() < Duration::from_secs(3));
        drop(guard);
        assert!(other.join().unwrap() < Duration::from_secs(4));
        // Without the lock the same write is retried for the full patience
        // (not waited out here); the count is back to zero.
        assert_eq!(HELD.with(|held| held.get()), 0);
    }

    #[test]
    fn an_unchanged_mode_is_not_rewritten() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("private");
        private_dir(&dir).unwrap();
        // A directory already 0700 needs no metadata write, which a full
        // filesystem may refuse (a chmod updates ctime even when unchanged).
        let before = fs::metadata(&dir).unwrap().ctime_nsec();
        private_dir(&dir).unwrap();
        assert_eq!(fs::metadata(&dir).unwrap().ctime_nsec(), before);
    }
}
