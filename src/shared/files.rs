//! Filesystem primitives for shared state: no-clobber publishing, SQLite
//! snapshots, managed-entry snapshots and file stamps.
use crate::{Error, Result, Safe, store};
use std::{
    ffi::OsStr,
    fs::{self, File},
    io::Read,
    os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

pub(super) fn entries(path: &Path) -> Result<Vec<PathBuf>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    fs::read_dir(path)
        .safe("cannot inspect provider state")?
        .map(|e| e.map(|e| e.path()).safe("cannot inspect provider state"))
        .collect()
}
pub(super) fn link(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}
pub(super) fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}
pub(super) fn dir(path: &Path) -> Result<bool> {
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
/// The files SQLite keeps beside a database in WAL mode, named by suffix.
pub(super) const WAL_SIDECARS: [&str; 2] = ["-wal", "-shm"];
/// Whether `name` is spelled like a SQLite database.
pub(super) fn sqlite_name(name: &OsStr) -> bool {
    name.to_str().is_some_and(|name| {
        [".sqlite", ".sqlite3", ".db"]
            .iter()
            .any(|suffix| name.ends_with(suffix))
    })
}
/// A regular SQLite database file in `dir` named `name`.
pub(super) fn sqlite_database(dir: &Path, name: &OsStr) -> bool {
    let path = dir.join(name);
    sqlite_name(name)
        && fs::symlink_metadata(&path).is_ok_and(|m| m.is_file())
        && sqlite_file(&path)
}
/// SQLite sidecars are transient and belong to the connection's database path.
pub(super) fn sqlite_sidecar(path: &Path) -> bool {
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
pub(super) fn sqlite_file(path: &Path) -> bool {
    let mut header = [0; 16];
    File::open(path)
        .and_then(|mut f| f.read_exact(&mut header))
        .is_ok()
        && &header == b"SQLite format 3\0"
}
/// Copy committed pages through SQLite so live WAL writes cannot tear snapshots.
pub(super) fn sqlite_snapshot(source: &Path, dest: &Path) -> Result<()> {
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
pub(super) fn new_file(source: &Path, dest: &Path) -> Result<()> {
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
pub(super) fn snapshot(source: &Path, dest: &Path, depth: usize) -> Result<()> {
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
/// A file's identity and version: atomic replacement changes the inode.
#[derive(Clone, PartialEq)]
pub(crate) struct Stamp(u64, u64, u64, i64, i64);
impl Stamp {
    pub(crate) fn from_metadata(m: &fs::Metadata) -> Self {
        Self(m.dev(), m.ino(), m.len(), m.mtime(), m.mtime_nsec())
    }
}
pub(crate) fn stamp(path: &Path) -> Option<Stamp> {
    let m = fs::symlink_metadata(path).ok()?;
    m.is_file().then(|| Stamp::from_metadata(&m))
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
