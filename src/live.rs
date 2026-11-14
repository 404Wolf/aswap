//! Per-session liveness records, never account identities or provider content.
use crate::{
    Error, Result, Safe,
    store::{self, Account, Store},
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};
#[derive(Serialize, Deserialize)]
struct Process {
    pid: u32,
    start: u64,
}
#[derive(Serialize, Deserialize)]
struct Record {
    boot: String,
    parent: Process,
    child: Option<Process>,
    #[serde(default)]
    history_only: bool,
    #[serde(default)]
    terminal: Option<String>,
    /// The run's launch view, so a refresh can hand the new token to it at
    /// once. Older records lack it; their runs still copy on their own tick.
    #[serde(default)]
    view: Option<PathBuf>,
}
fn process(pid: u32) -> Option<Process> {
    let text = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, tail) = text.rsplit_once(')')?;
    let fields: Vec<_> = tail.split_whitespace().collect();
    if matches!(*fields.first()?, "Z" | "X") {
        return None;
    }
    Some(Process {
        pid,
        start: fields.get(19)?.parse().ok()?,
    })
}
fn alive(p: &Process) -> bool {
    if let Some(current) = process(p.pid) {
        return p.start == current.start;
    }
    // Permission errors are not evidence that another process has exited.
    match fs::metadata(format!("/proc/{}", p.pid)) {
        Ok(_) => fs::read_to_string(format!("/proc/{}/stat", p.pid)).is_err(),
        Err(error) => error.kind() != std::io::ErrorKind::NotFound,
    }
}
fn boot() -> Result<String> {
    fs::read_to_string("/proc/sys/kernel/random/boot_id").safe("cannot identify system boot")
}
fn directory(store: &Store, a: &Account) -> Result<PathBuf> {
    store::label(&a.label)?;
    Ok(store
        .root
        .join("live")
        .join(a.provider.as_str())
        .join(&a.label))
}
pub struct Lease {
    root: PathBuf,
    path: PathBuf,
}
impl Drop for Lease {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}
impl Lease {
    /// Register the child before supervision starts. If the supervisor is killed,
    /// a surviving provider still protects the profile until that child exits.
    pub fn child(&self, pid: u32) -> Result<()> {
        self.record_child(pid, false)
    }
    pub fn history_child(&self, pid: u32) -> Result<()> {
        self.record_child(pid, true)
    }
    fn record_child(&self, pid: u32, history_only: bool) -> Result<()> {
        self.update(|record| {
            record.child = process(pid);
            record.history_only = history_only;
        })
    }
    /// Name the launch view this run's provider reads its credentials from.
    pub fn view(&self, path: &Path) -> Result<()> {
        self.update(|record| record.view = Some(path.to_owned()))
    }
    fn update(&self, change: impl FnOnce(&mut Record)) -> Result<()> {
        let store = Store::open(self.root.clone())?;
        let _lock = store.lock()?;
        let mut record: Record = store::json_read(&self.path)?;
        change(&mut record);
        store::json_write(&self.path, &record)
    }
}
/// Caller holds the store lock, serializing registration with mutations.
pub(crate) fn register(store: &Store, a: &Account) -> Result<Lease> {
    let dir = directory(store, a)?;
    store::private_dir(&dir)?;
    let path = loop {
        let pending = tempfile::Builder::new()
            .prefix("session-")
            .tempfile_in(&dir)
            .safe("cannot register live session")?;
        let path = pending.path().with_extension("json");
        // Registrations share the store lock; never replace an existing record
        // even in the unlikely event of a random-name collision.
        let available = !path.exists();
        drop(pending);
        if available {
            break path;
        }
    };
    let record = Record {
        boot: boot()?,
        parent: process(std::process::id()).ok_or(Error("cannot identify live session"))?,
        child: None,
        history_only: false,
        terminal: std::env::var("HC_TERMINAL_ID").ok(),
        view: None,
    };
    store::json_write(&path, &record)?;
    Ok(Lease {
        root: store.root.clone(),
        path,
    })
}
/// Caller holds the store lock. Dead supervisors/children and old boot records
/// are collected automatically, including PID reuse and zombies.
pub(crate) fn count(store: &Store, a: &Account) -> Result<usize> {
    let dir = directory(store, a)?;
    store::no_links(&dir)?;
    if !dir.exists() {
        return Ok(0);
    }
    let current_boot = boot()?;
    let mut count = 0;
    for entry in fs::read_dir(&dir).safe("cannot inspect live sessions")? {
        let path = entry.safe("cannot inspect live session")?.path();
        store::no_links(&path)?;
        // Under the store lock, any unpublished temporary file is an orphan.
        if path.extension().is_none_or(|extension| extension != "json") {
            fs::remove_file(path).safe("cannot clean stale session temporary file")?;
            continue;
        }
        let record: Record = match store::json_read(&path) {
            Ok(record) => record,
            Err(_) if !path.exists() => continue,
            Err(error) => return Err(error),
        };
        if record.boot == current_boot
            && (alive(&record.parent) || record.child.as_ref().is_some_and(alive))
        {
            count += 1;
        } else {
            fs::remove_file(path).safe("cannot clean stale session")?;
        }
    }
    Ok(count)
}

/// The launch views of this account's live runs. Reads only: no store lock,
/// and stale records are left for `count` to collect. A view may have moved
/// to another account since; the caller checks under the view's own lock.
pub(crate) fn views(store: &Store, a: &Account) -> Vec<PathBuf> {
    let (Ok(dir), Ok(current_boot)) = (directory(store, a), boot()) else {
        return Vec::new();
    };
    if store::no_links(&dir).is_err() {
        return Vec::new();
    }
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            if path.extension().is_none_or(|extension| extension != "json") {
                return None;
            }
            let record: Record = store::json_read(&path).ok()?;
            (record.boot == current_boot
                && (alive(&record.parent) || record.child.as_ref().is_some_and(alive)))
            .then_some(record.view)
            .flatten()
        })
        .collect()
}

/// Local live sessions, including the actual credential profile after failover.
pub fn sessions(store: &Store) -> Result<Vec<serde_json::Value>> {
    let _lock = store.lock()?;
    let mut rows = Vec::new();
    for account in store.load()?.accounts {
        count(store, &account)?;
        let dir = directory(store, &account)?;
        if !dir.exists() {
            continue;
        }
        for entry in fs::read_dir(dir).safe("cannot inspect live sessions")? {
            let path = entry.safe("cannot inspect live session")?.path();
            let record: Record = store::json_read(&path)?;
            if !record.history_only {
                let terminal = record.terminal.or_else(|| {
                    let bytes = fs::read(format!("/proc/{}/environ", record.parent.pid)).ok()?;
                    bytes.split(|b| *b == 0).find_map(|entry| {
                        entry
                            .strip_prefix(b"HC_TERMINAL_ID=")
                            .and_then(|value| std::str::from_utf8(value).ok())
                            .map(str::to_owned)
                    })
                });
                rows.push(serde_json::json!({"provider": account.provider, "account": account.label, "supervisor_pid": record.parent.pid, "child_pid": record.child.map(|p|p.pid), "terminal": terminal}));
            }
        }
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Provider;
    #[test]
    fn stale_pid_reuse_old_boot_and_orphan_temporary_records_are_collected() {
        let t = tempfile::tempdir().unwrap();
        let store = Store::open(t.path().join("store")).unwrap();
        let a = Account {
            provider: Provider::Claude,
            label: "synthetic".into(),
        };
        let _lock = store.lock().unwrap();
        let lease = register(&store, &a).unwrap();
        assert_eq!(count(&store, &a).unwrap(), 1);
        let mut record: Record = store::json_read(&lease.path).unwrap();
        record.parent.start += 1;
        store::json_write(&lease.path, &record).unwrap();
        assert_eq!(count(&store, &a).unwrap(), 0);
        assert!(!lease.path.exists());
        record.parent = Process {
            pid: u32::MAX,
            start: 0,
        };
        record.child = process(std::process::id());
        store::json_write(&lease.path, &record).unwrap();
        assert_eq!(count(&store, &a).unwrap(), 1);
        record.boot = "synthetic-previous-boot".into();
        store::json_write(&lease.path, &record).unwrap();
        store::atomic(
            &directory(&store, &a).unwrap().join(".tmp-orphan"),
            b"synthetic interrupted write",
        )
        .unwrap();
        assert_eq!(count(&store, &a).unwrap(), 0);
        assert_eq!(
            fs::read_dir(directory(&store, &a).unwrap())
                .unwrap()
                .count(),
            0
        );
    }
}

#[cfg(test)]
mod session_tests {
    use super::*;
    #[test]
    fn history_leases_are_not_reported_as_active_credentials() {
        let t = tempfile::tempdir().unwrap();
        let store = Store::open(t.path().join("store")).unwrap();
        let a = Account {
            provider: crate::provider::Provider::Claude,
            label: "synthetic".into(),
        };
        let lease = {
            let _lock = store.lock().unwrap();
            let mut state = store.load().unwrap();
            state.accounts.push(a.clone());
            store.save(&state).unwrap();
            register(&store, &a).unwrap()
        };
        lease.child(std::process::id()).unwrap();
        let rows = sessions(&store).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["account"], "synthetic");
        lease.history_child(std::process::id()).unwrap();
        assert!(sessions(&store).unwrap().is_empty());
    }
}

#[cfg(test)]
mod hand_out_tests {
    use super::*;
    use serde_json::{Value, json};
    #[test]
    fn hand_out_skips_dead_viewless_lockless_and_vanished_views() {
        let t = tempfile::tempdir().unwrap();
        let store = Store::open(t.path().join("store")).unwrap();
        let a = Account {
            provider: crate::provider::Provider::Claude,
            label: "one".into(),
        };
        let old = json!({"claudeAiOauth":{"accessToken":"synthetic-old","refreshToken":"synthetic-old-sentinel"}});
        let view = |name: &str, lock: bool| {
            let path = store.root.join("runtimes/claude/one").join(name);
            store::json_write(&path.join(".credentials.json"), &old).unwrap();
            if lock {
                fs::write(path.with_extension("lock"), "one").unwrap();
            }
            path
        };
        let live = view("run-live", true);
        // A run from an older release has no view lock.
        let lockless = view("run-lockless", false);
        let dead = view("run-dead", true);
        let vanished = view("run-vanished", true);
        fs::remove_dir_all(&vanished).unwrap();
        let _lock = store.lock().unwrap();
        let _leases: Vec<Lease> = [
            Some(&live),
            Some(&lockless),
            Some(&dead),
            Some(&vanished),
            None,
        ]
        .into_iter()
        .map(|path| {
            let lease = register(&store, &a).unwrap();
            if let Some(path) = path {
                let mut record: Record = store::json_read(&lease.path).unwrap();
                record.view = Some(path.clone());
                if path == &dead {
                    record.parent.start += 1;
                }
                store::json_write(&lease.path, &record).unwrap();
            }
            lease
        })
        .collect();
        let new = json!({"claudeAiOauth":{"accessToken":"synthetic-new","refreshToken":"synthetic-real-refresh","expiresAt":1000}});
        assert_eq!(crate::shared::hand_out(&store, &a, &new), 1);
        let token = |path: &PathBuf| -> Value {
            store::json_read::<Value>(&path.join(".credentials.json")).unwrap()["claudeAiOauth"]
                ["accessToken"]
                .clone()
        };
        assert_eq!(token(&live), "synthetic-new");
        assert_eq!(token(&lockless), "synthetic-old");
        assert_eq!(token(&dead), "synthetic-old");
        assert!(!vanished.exists());
        let handed: Value = store::json_read(&live.join(".credentials.json")).unwrap();
        assert_eq!(
            handed["claudeAiOauth"]["refreshToken"],
            "aswap-owner-managed"
        );
    }
}
