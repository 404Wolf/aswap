//! pi's lock on its `auth.json`, as pi takes it (proper-lockfile with
//! `realpath: false`): a directory `<file>.lock` beside the file, created
//! atomically. A holder refreshes the directory's mtime while it holds the
//! lock, so one untouched for longer than [`STALE`] belongs to a pi that died
//! holding it, and is broken, as proper-lockfile breaks it.
use std::{
    ffi::OsString,
    io,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

/// The longest staleness pi uses (30s for its async lock, 10s for its sync
/// one): a lock older than both is abandoned.
pub const STALE: Duration = Duration::from_secs(30);

/// Why the lock could not be taken.
#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("pi's auth.json is locked by a running pi")]
    Held,
    #[error("cannot lock pi's auth.json: {0}")]
    Io(#[from] io::Error),
}

/// pi's lock on one file, released on drop.
pub struct PiLock {
    dir: PathBuf,
}
impl PiLock {
    /// Take the lock on `file`, waiting up to `patience` for a live holder
    /// and breaking a stale lock.
    pub fn acquire(file: &Path, patience: Duration) -> Result<Self, LockError> {
        let mut dir = OsString::from(file.as_os_str());
        dir.push(".lock");
        let dir = PathBuf::from(dir);
        let deadline = Instant::now() + patience;
        loop {
            match std::fs::create_dir(&dir) {
                Ok(()) => return Ok(Self { dir }),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e.into()),
            }
            if stale(&dir) {
                match std::fs::remove_dir(&dir) {
                    Ok(()) => continue,
                    // Another process broke or released it first.
                    Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(e.into()),
                }
            }
            if Instant::now() >= deadline {
                return Err(LockError::Held);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}
impl Drop for PiLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.dir);
    }
}
fn stale(dir: &Path) -> bool {
    std::fs::metadata(dir)
        .and_then(|m| m.modified())
        .is_ok_and(|at| {
            SystemTime::now()
                .duration_since(at)
                .is_ok_and(|age| age > STALE)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_held_lock_waits_then_fails_and_a_stale_one_is_broken() {
        let t = tempfile::tempdir().unwrap();
        let file = t.path().join("auth.json");
        let lock = t.path().join("auth.json.lock");
        {
            let _held = PiLock::acquire(&file, Duration::ZERO).unwrap();
            assert!(lock.is_dir());
            // A second taker (a fresh lock: pi is running) gives up.
            let started = Instant::now();
            assert!(matches!(
                PiLock::acquire(&file, Duration::from_millis(200)),
                Err(LockError::Held)
            ));
            assert!(started.elapsed() >= Duration::from_millis(200));
        }
        assert!(!lock.exists(), "dropping the guard releases the lock");
        // A pi that died holding it: the lock is a minute old.
        std::fs::create_dir(&lock).unwrap();
        let old = SystemTime::now() - Duration::from_secs(60);
        std::fs::File::open(&lock)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let taken = PiLock::acquire(&file, Duration::ZERO).unwrap();
        assert!(lock.is_dir());
        drop(taken);
        assert!(!lock.exists());
    }
}
