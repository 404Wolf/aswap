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
}
/// How auto selection ranks eligible accounts.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Prefer {
    /// The lowest used percentage.
    #[default]
    Headroom,
    /// The soonest weekly reset, so quota that would expire unused goes first.
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
            prefer: Prefer::Headroom,
        }
    }
}
#[derive(Default, Serialize, Deserialize)]
pub struct State {
    pub accounts: Vec<Account>,
    pub active: BTreeMap<Provider, String>,
    pub auto: BTreeMap<Provider, Auto>,
}
pub struct Store {
    pub root: PathBuf,
    pub(crate) sync_policy_path: PathBuf,
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
/// Reject symlinks at every existing path component, including ancestors.
pub fn no_links(path: &Path) -> Result<()> {
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(Error(
            "an absolute path without parent traversal is required",
        ));
    }
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
        return Err(Error("private file exceeds size limit"));
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
    pub fn from_env() -> Result<Self> {
        let base = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or(crate::provider::home()?.join(".local/share"));
        Self::open(base.join("aswap"))
    }
    pub fn open(root: PathBuf) -> Result<Self> {
        no_links(&root)?;
        // Public checkout must never become an account store, even by accident.
        if root.ancestors().any(|p| p.join(".git").exists()) {
            return Err(Error("account stores must live outside a Git checkout"));
        }
        private_dir(&root)?;
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or(crate::provider::home()?.join(".config"));
        Ok(Self {
            root,
            sync_policy_path: config.join("aswap/sync-policy.json"),
        })
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
    pub fn load(&self) -> Result<State> {
        let p = self.root.join("state.json");
        let state: State = if p.exists() {
            json_read(&p)?
        } else {
            State::default()
        };
        for a in &state.accounts {
            label(&a.label)?;
        }
        for l in state.active.values() {
            label(l)?;
        }
        for a in state.auto.values() {
            threshold(a.threshold)?;
        }
        Ok(state)
    }
    pub fn save(&self, state: &State) -> Result<()> {
        json_write(&self.root.join("state.json"), state)
    }
    pub fn profile(&self, provider: Provider, name: &str) -> Result<PathBuf> {
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
