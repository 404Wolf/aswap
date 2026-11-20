//! GC-side repair for rows whose absolute rollout path goes through this view.
use super::{
    Pressure,
    files::{Dir, Home},
};
use crate::{Error, Result, Safe, session::valid_id, shared};
use rusqlite::{Connection, OpenFlags, TransactionBehavior, params};
use std::{
    collections::HashSet,
    ffi::OsStr,
    fs::File,
    io::ErrorKind,
    os::unix::{
        ffi::OsStrExt,
        fs::{FileExt, MetadataExt},
    },
    path::{Component, Path, PathBuf},
    time::Duration,
};

#[derive(Default)]
pub(super) struct Backups(HashSet<(u64, u64)>);

pub(super) fn check(
    home: &Home,
    view: &Path,
    apply: Option<(&mut Backups, &Path, &mut Vec<String>)>,
    mut verify: impl FnMut(&Path, &mut Pressure<'_>) -> Result<()>,
    pressure: &mut Pressure<'_>,
) -> Result<(u64, u64)> {
    let mut apply = apply;
    let mut rewritten = 0;
    let mut backup_bytes = 0;
    for name in home
        .dir
        .names()
        .safe("cannot list shared Codex index databases")?
    {
        let Some(name) = name.to_str().filter(|name| {
            name.strip_prefix("state_")
                .and_then(|n| n.strip_suffix(".sqlite"))
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        }) else {
            continue;
        };
        pressure.check(false)?;
        let (file, metadata) = home
            .dir
            .file(OsStr::new(name))
            .safe("cannot open shared Codex index database")?;
        let file_path = home.path.join(name);
        let (wal, shm) = sidecars(&home.dir, name)?;
        let mut mode = [0; 2];
        file.read_exact_at(&mut mode, 18)
            .safe("cannot read shared Codex index database header")?;

        // A normal read-only SQLite open can create WAL sidecars. With no
        // sidecars present, preview can read the main file without creating
        // any; a WAL whose shared-memory file is missing is ambiguous.
        let idle_wal = mode == [2, 2] && !wal && !shm;
        if mode == [2, 2] && wal != shm {
            return Err(Error(
                "cannot inspect Codex index with incomplete WAL sidecars",
            ));
        }
        let db = if idle_wal {
            use std::os::fd::AsRawFd;
            let uri = format!("file:/proc/self/fd/{}?immutable=1", file.as_raw_fd());
            Connection::open_with_flags(
                uri,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
            )
        } else {
            open_index(&file_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        }
        .safe("cannot open shared Codex index")?;
        db.busy_timeout(Duration::from_secs(5))
            .safe("cannot set Codex index busy timeout")?;
        let before = rows(&db, view)?;
        for (id, stored) in &before {
            pressure.check(false)?;
            verify(relative_rollout(view, id, stored)?, pressure)?;
        }
        if idle_wal
            && (sidecars(&home.dir, name)? != (false, false)
                || file
                    .metadata()
                    .safe("cannot recheck Codex index database")?
                    .modified()
                    .safe("cannot read Codex index modification time")?
                    != metadata
                        .modified()
                        .safe("cannot read Codex index modification time")?)
        {
            return Err(Error("Codex index changed during preview"));
        }
        let Some((backups, archive, reported)) = apply.as_mut() else {
            continue;
        };
        if before.is_empty() {
            continue;
        }
        drop(db);
        let key = (metadata.dev(), metadata.ino());
        if !backups.0.contains(&key) {
            let (backup, bytes) = backup_index(&file_path, name, archive)?;
            reported.push(backup.to_string_lossy().into_owned());
            backup_bytes += bytes;
            backups.0.insert(key);
        }

        pressure.check(false)?;
        let (_, current) = home
            .dir
            .file(OsStr::new(name))
            .safe("cannot recheck shared Codex index")?;
        if (current.dev(), current.ino()) != key {
            return Err(Error("shared Codex index was replaced during backup"));
        }
        let mut db = open_index(&file_path, OpenFlags::SQLITE_OPEN_READ_WRITE)
            .safe("cannot open shared Codex index for repair")?;
        db.busy_timeout(Duration::from_secs(5))
            .safe("cannot set Codex index busy timeout")?;
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .safe("cannot lock shared Codex index for repair")?;
        if rows(&tx, view)? != before {
            return Err(Error("Codex index references changed during repair"));
        }
        for (id, old) in &before {
            let replacement = home.path.join(relative_rollout(view, id, old)?);
            let replacement = replacement
                .to_str()
                .ok_or(Error("shared Codex rollout path is not UTF-8"))?;
            let updated = tx
                .execute(
                    "UPDATE threads SET rollout_path = ?1 WHERE id = ?2 AND rollout_path = ?3",
                    params![replacement, id, old],
                )
                .safe("cannot rewrite shared Codex index row")?;
            if updated != 1 {
                return Err(Error("Codex index row changed during repair"));
            }
        }
        tx.commit().safe("cannot commit Codex index repair")?;
        rewritten += before.len() as u64;
    }
    Ok((rewritten, backup_bytes))
}

fn open_index(path: &Path, access: OpenFlags) -> rusqlite::Result<Connection> {
    Connection::open_with_flags(
        path,
        access | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
}

fn sidecars(home: &Dir, database: &str) -> Result<(bool, bool)> {
    let present = |suffix| match home.file(OsStr::new(&format!("{database}{suffix}"))) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(_) => Err(Error("unsafe or unreadable shared Codex index sidecar")),
    };
    let _ = present("-journal")?;
    Ok((present("-wal")?, present("-shm")?))
}

fn rows(db: &Connection, view: &Path) -> Result<Vec<(String, String)>> {
    let mut columns = db
        .prepare("SELECT name FROM pragma_table_info('threads')")
        .safe("cannot inspect Codex index schema")?;
    let columns: Vec<String> = columns
        .query_map([], |row| row.get(0))
        .and_then(Iterator::collect)
        .safe("cannot read Codex index schema")?;
    if columns.is_empty() {
        return Ok(Vec::new());
    }
    if !["id", "rollout_path"]
        .iter()
        .all(|c| columns.iter().any(|have| have == c))
    {
        return Err(Error("unrecognized Codex index schema"));
    }
    let view = view.as_os_str().as_bytes();
    let mut prefix = view.to_vec();
    prefix.push(b'/');
    let mut query = db
        .prepare(
            "SELECT id, rollout_path FROM threads
         WHERE substr(CAST(rollout_path AS BLOB), 1, length(?1)) = ?1
            OR CAST(rollout_path AS BLOB) = ?2
         ORDER BY id, rollout_path",
        )
        .safe("cannot prepare Codex index query")?;
    query
        .query_map(params![prefix, view], |row| Ok((row.get(0)?, row.get(1)?)))
        .and_then(Iterator::collect)
        .safe("cannot read a Codex index row for this view")
}

fn relative_rollout<'a>(view: &Path, id: &str, stored: &'a str) -> Result<&'a Path> {
    const MALFORMED: Error = Error("malformed Codex index row for this view");
    if !valid_id(id) {
        return Err(MALFORMED);
    }
    let path = Path::new(stored).strip_prefix(view).safe(MALFORMED.0)?;
    let mut parts = path.components();
    if !matches!(parts.next(), Some(Component::Normal(root)) if root == "sessions" || root == "archived_sessions")
        || !parts.all(|part| matches!(part, Component::Normal(_)))
        || !path
            .file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|name| name.ends_with(&format!("{id}.jsonl")))
    {
        return Err(MALFORMED);
    }
    Ok(path)
}

fn backup_index(source: &Path, name: &str, archive: &Path) -> Result<(PathBuf, u64)> {
    let folder = archive.join("codex/index-backups");
    let root = Dir::open(Path::new("/")).safe("cannot open backup filesystem")?;
    let dir = root
        .descend(
            folder.strip_prefix("/").safe("backup must be absolute")?,
            true,
        )
        .safe("cannot create Codex index backup directory")?;
    let temp = tempfile::Builder::new()
        .prefix(".pending-")
        .tempfile_in(dir.path())
        .safe("cannot stage Codex index backup")?;
    shared::sqlite_snapshot(source, temp.path())?;
    let backup = File::open(temp.path()).safe("cannot open Codex index backup")?;
    backup.sync_all().safe("cannot sync Codex index backup")?;
    let bytes = backup
        .metadata()
        .safe("cannot inspect Codex index backup")?
        .len();
    let suffix = temp
        .path()
        .file_name()
        .and_then(OsStr::to_str)
        .and_then(|name| name.strip_prefix(".pending-"))
        .ok_or(Error("invalid backup filename"))?;
    let name = format!("{name}-{suffix}.sqlite");
    temp.persist_noclobber(dir.path().join(&name))
        .safe("cannot publish Codex index backup")?;
    dir.0
        .sync_all()
        .safe("cannot sync Codex index backup directory")?;
    Ok((folder.join(name), bytes))
}
