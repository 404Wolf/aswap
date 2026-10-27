//! Per-session liveness records, never account identities or provider content.
use crate::{
    Error, Result, Safe,
    store::{self, Account, Store},
};
use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf};
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
        let store = Store::open(self.root.clone())?;
        let _lock = store.lock()?;
        let mut record: Record = store::json_read(&self.path)?;
        record.child = process(pid);
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
