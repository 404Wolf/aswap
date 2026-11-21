//! Codex keeps a thread index in its state database (`state_<n>.sqlite`,
//! table `threads`), and each row's `rollout_path` is the transcript as Codex
//! spelled it: `$CODEX_HOME/sessions/...`. In a launch view, `sessions` and
//! `archived_sessions` are links to the shared home, and so is the database.
//! A run therefore records the view's spelling in the shared index, and
//! `codex resume` loses the transcript once the view is removed.
//!
//! A repair rewrites such a row to the home's spelling of the same file, and
//! only when that file exists, its name carries the row's thread ID, and,
//! while the view still exists, it is the very file the view names. The
//! scan only reads.
use super::super::index::{Finding, Problem, Scope};
use crate::session::SessionId;
use rusqlite::{Connection, OpenFlags, params};
use std::{
    os::unix::fs::MetadataExt,
    path::{Component, Path, PathBuf},
    time::Duration,
};

/// The history directories a launch view links to the home.
const LINKED: [&str; 2] = ["sessions", "archived_sessions"];
/// How long a run-end repair waits for a running Codex to release the
/// database.
const BUSY: Duration = Duration::from_secs(5);
/// How long the sweep on the launch path waits: a busy database is left to
/// the next launch rather than delaying this one.
const SWEEP_BUSY: Duration = Duration::from_millis(250);

/// What can stop a scan or a repair.
#[derive(Debug, thiserror::Error)]
pub enum RolloutError {
    #[error("cannot read the Codex home")]
    Home(#[source] std::io::Error),
    #[error("cannot open Codex's state database")]
    Open(#[source] rusqlite::Error),
    #[error("cannot read Codex's thread index")]
    Read(#[source] rusqlite::Error),
    #[error("cannot update Codex's thread index")]
    Write(#[source] rusqlite::Error),
}
impl From<RolloutError> for crate::Error {
    fn from(e: RolloutError) -> Self {
        crate::Error(match e {
            RolloutError::Home(_) => "cannot read the Codex home",
            RolloutError::Open(_) => "cannot open Codex's state database",
            RolloutError::Read(_) => "cannot read Codex's thread index",
            RolloutError::Write(_) => "cannot update Codex's thread index",
        })
    }
}

/// The spellings of the views root a row may use: as aswap spells it, and
/// resolved (Codex may canonicalize `CODEX_HOME`, so a symlink above the
/// store changes the recorded prefix).
struct Views(Vec<PathBuf>);
impl Views {
    fn new(views: &Path) -> Self {
        let mut spellings = vec![views.to_path_buf()];
        if let Ok(real) = std::fs::canonicalize(views)
            && real != views
        {
            spellings.push(real);
        }
        Self(spellings)
    }
}
/// Whether two spellings name the same launch view.
fn same_view(a: &Path, b: &Path) -> bool {
    a == b
        || matches!(
            (std::fs::canonicalize(a), std::fs::canonicalize(b)),
            (Ok(a), Ok(b)) if a == b
        )
}

/// A `rollout_path` that goes through a launch view under the views root.
#[derive(Debug, PartialEq, Eq)]
struct ViewPath {
    /// The launch view (`<views>/<label>/run-*`).
    view: PathBuf,
    /// The same file under the home.
    canonical: PathBuf,
}
impl ViewPath {
    /// `<views>/<label>/<run>/<sessions|archived_sessions>/<rest>`, with
    /// plain components only.
    fn parse(views: &Views, home: &Path, path: &Path) -> Option<Self> {
        let (views, rest) = views
            .0
            .iter()
            .find_map(|views| Some((views, path.strip_prefix(views).ok()?)))?;
        let mut parts = rest.components();
        let mut next = || match parts.next() {
            Some(Component::Normal(part)) => Some(part),
            _ => None,
        };
        let (label, run, dir) = (next()?, next()?, next()?);
        let dir = LINKED.into_iter().find(|linked| dir == *linked)?;
        let tail = parts.as_path();
        if tail.as_os_str().is_empty()
            || !tail.components().all(|c| matches!(c, Component::Normal(_)))
        {
            return None;
        }
        Some(Self {
            view: views.join(label).join(run),
            canonical: home.join(dir).join(tail),
        })
    }
}

/// A shared lock on the runtimes directory (the parent of `views`), held
/// while the sweep runs; none while `aswap gc` holds it exclusively. A
/// missing runtimes directory has nothing to sweep.
fn gc_idle(views: &Path) -> Option<std::fs::File> {
    let runtimes = std::fs::File::open(views.parent()?).ok()?;
    fs2::FileExt::try_lock_shared(&runtimes).ok()?;
    Some(runtimes)
}
/// Where `aswap gc` moves `view` while it retires it.
fn retiring(view: &Path) -> PathBuf {
    let mut name = std::ffi::OsString::from(crate::retention::RETIRING_PREFIX);
    name.push(view.file_name().unwrap_or_default());
    view.with_file_name(name)
}

/// Whether `path` is a regular file whose name carries thread `id`.
fn names_thread(path: &Path, id: &SessionId) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(&format!("{id}.jsonl")))
        && std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file())
}

/// Whether two paths name the same file.
fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

/// Codex's state databases in `home`: `state_<n>.sqlite` regular files.
fn databases(home: &Path) -> Result<Vec<PathBuf>, RolloutError> {
    let entries = match std::fs::read_dir(home) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(RolloutError::Home(e)),
    };
    let mut found: Vec<_> = entries
        .flatten()
        .filter(|entry| {
            entry.file_name().to_str().is_some_and(|name| {
                name.strip_prefix("state_")
                    .and_then(|n| n.strip_suffix(".sqlite"))
                    .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            }) && entry.file_type().is_ok_and(|t| t.is_file())
        })
        .map(|entry| entry.path())
        .collect();
    found.sort();
    Ok(found)
}

/// One thread row: its ID, and its `rollout_path` as stored.
struct Row {
    id: SessionId,
    stored: String,
    path: PathBuf,
}
/// The thread rows of a database's index; none when the database has no
/// such table (another Codex version). Rows without a valid ID or an
/// absolute text path are skipped, never fatal.
fn rows(db: &Connection) -> Result<Vec<Row>, RolloutError> {
    let columns: Vec<String> = db
        .prepare("SELECT name FROM pragma_table_info('threads')")
        .and_then(|mut q| q.query_map([], |row| row.get(0))?.collect())
        .map_err(RolloutError::Read)?;
    if !["id", "rollout_path"]
        .iter()
        .all(|c| columns.iter().any(|have| have == c))
    {
        return Ok(Vec::new());
    }
    let mut query = db
        .prepare("SELECT id, rollout_path FROM threads")
        .map_err(RolloutError::Read)?;
    let text = |value: rusqlite::types::ValueRef| match value {
        rusqlite::types::ValueRef::Text(bytes) => {
            std::str::from_utf8(bytes).ok().map(str::to_owned)
        }
        _ => None,
    };
    let rows = query
        .query_map([], |row| Ok((text(row.get_ref(0)?), text(row.get_ref(1)?))))
        .map_err(RolloutError::Read)?;
    let mut out = Vec::new();
    for row in rows {
        let (Some(id), Some(stored)) = row.map_err(RolloutError::Read)? else {
            continue;
        };
        let path = PathBuf::from(&stored);
        if let Some(id) = SessionId::parse(&id)
            && path.is_absolute()
        {
            out.push(Row { id, stored, path });
        }
    }
    Ok(out)
}

/// Every thread row that names a missing transcript or one through a launch
/// view under `views`. Opens the databases read-only and changes nothing.
pub(super) fn scan(home: &Path, views: &Path) -> Result<Vec<Finding>, RolloutError> {
    let views = Views::new(views);
    let mut findings = Vec::new();
    for database in databases(home)? {
        let db = Connection::open_with_flags(
            &database,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(RolloutError::Open)?;
        db.busy_timeout(BUSY).map_err(RolloutError::Open)?;
        for Row { id, path, .. } in rows(&db)? {
            let through = ViewPath::parse(&views, home, &path);
            let problem = if !path.exists() {
                Problem::Dangling
            } else if through.is_some() {
                Problem::InView
            } else {
                continue;
            };
            let canonical = through
                .map(|t| t.canonical)
                .filter(|canonical| names_thread(canonical, &id));
            findings.push(Finding {
                database: database.clone(),
                id,
                path,
                problem,
                canonical,
            });
        }
    }
    Ok(findings)
}

/// Rewrite thread rows that name a transcript through a launch view under
/// `views` (limited by `scope`) to the same file under `home`. Returns how
/// many rows changed.
pub(super) fn repair(home: &Path, views: &Path, scope: Scope) -> Result<usize, RolloutError> {
    // The sweep stands aside while `aswap gc` runs: it holds the runtimes
    // directory's lock exclusively, renames a view to `.retiring-<run>` and
    // repairs that view's rows itself.
    let _gc = match scope {
        Scope::Gone => match gc_idle(views) {
            Some(lock) => Some(lock),
            None => return Ok(0),
        },
        Scope::View(_) => None,
    };
    let views = Views::new(views);
    let busy = match scope {
        Scope::View(_) => BUSY,
        Scope::Gone => SWEEP_BUSY,
    };
    let mut changed = 0;
    for database in databases(home)? {
        let mut db = Connection::open_with_flags(
            &database,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(RolloutError::Open)?;
        db.busy_timeout(busy).map_err(RolloutError::Open)?;
        let fixes: Vec<_> = rows(&db)?
            .into_iter()
            .filter_map(|Row { id, stored, path }| {
                let through = ViewPath::parse(&views, home, &path)?;
                let in_scope = match scope {
                    Scope::View(view) => same_view(&through.view, view),
                    Scope::Gone => !through.view.exists() && !retiring(&through.view).exists(),
                };
                // While the view exists, its path must still be this file.
                // (If the home's sessions/ is itself a link, the view holds a
                // copy instead, nothing matches here, and the transcript is
                // left to the launch sweep once sync has published it.)
                let consistent = !path.exists() || same_file(&path, &through.canonical);
                let new = through.canonical.to_str()?.to_owned();
                (in_scope && consistent && names_thread(&through.canonical, &id))
                    .then_some((id, stored, new))
            })
            .collect();
        if fixes.is_empty() {
            continue;
        }
        let tx = db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(RolloutError::Write)?;
        for (id, old, new) in &fixes {
            // Only the row as read: Codex may have moved it meanwhile.
            changed += tx
                .execute(
                    "UPDATE threads SET rollout_path = ?1 WHERE id = ?2 AND rollout_path = ?3",
                    params![new, id.as_str(), old],
                )
                .map_err(RolloutError::Write)?;
        }
        tx.commit().map_err(RolloutError::Write)?;
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "01a1f1fe-7601-7603-9212-031af3c72201";
    const OTHER: &str = "01a1f1fe-c795-74e0-b9ad-dfa05f6b48a3";

    /// A home with a state database, a transcript for each ID, and a view
    /// under `views` whose `sessions` links to the home's.
    struct Fixture {
        _temp: tempfile::TempDir,
        home: PathBuf,
        views: PathBuf,
        view: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let home = temp.path().join("codex");
            let views = temp.path().join("aswap/runtimes/codex");
            let view = views.join("work/run-AbC123");
            let day = home.join("sessions/2026/11/19");
            std::fs::create_dir_all(&day).unwrap();
            std::fs::create_dir_all(&view).unwrap();
            std::os::unix::fs::symlink(home.join("sessions"), view.join("sessions")).unwrap();
            for id in [ID, OTHER] {
                std::fs::write(day.join(Self::name(id)), "{}\n").unwrap();
            }
            let db = Connection::open(home.join("state_5.sqlite")).unwrap();
            db.execute_batch(
                "PRAGMA journal_mode=WAL;
                 CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL, title TEXT NOT NULL DEFAULT '');",
            )
            .unwrap();
            Self {
                _temp: temp,
                home,
                views,
                view,
            }
        }
        fn name(id: &str) -> String {
            format!("rollout-2026-11-19T04-09-07-{id}.jsonl")
        }
        fn through(&self, root: &Path, id: &str) -> PathBuf {
            root.join("sessions/2026/11/19").join(Self::name(id))
        }
        fn row(&self, id: &str, path: &Path) {
            Connection::open(self.home.join("state_5.sqlite"))
                .unwrap()
                .execute(
                    "INSERT OR REPLACE INTO threads (id, rollout_path) VALUES (?1, ?2)",
                    params![id, path.to_str().unwrap()],
                )
                .unwrap();
        }
        fn path_of(&self, id: &str) -> PathBuf {
            let db = Connection::open(self.home.join("state_5.sqlite")).unwrap();
            let path: String = db
                .query_row(
                    "SELECT rollout_path FROM threads WHERE id = ?1",
                    [id],
                    |r| r.get(0),
                )
                .unwrap();
            path.into()
        }
    }

    #[test]
    fn only_paths_through_a_views_linked_history_parse() {
        let (views, home) = (&Views(vec!["/s/runtimes/codex".into()]), Path::new("/h"));
        let path = Path::new("/s/runtimes/codex/a/run-x/sessions/2026/r.jsonl");
        assert_eq!(
            ViewPath::parse(views, home, path),
            Some(ViewPath {
                view: "/s/runtimes/codex/a/run-x".into(),
                canonical: "/h/sessions/2026/r.jsonl".into(),
            })
        );
        let archived = Path::new("/s/runtimes/codex/a/run-x/archived_sessions/r.jsonl");
        assert_eq!(
            ViewPath::parse(views, home, archived).unwrap().canonical,
            Path::new("/h/archived_sessions/r.jsonl")
        );
        for other in [
            "/h/sessions/2026/r.jsonl",
            "/s/runtimes/codex/a/run-x/auth.json",
            "/s/runtimes/codex/a/run-x/sessions",
            "/s/runtimes/codex/a/run-x/sessions/../../../../etc/passwd",
        ] {
            assert_eq!(
                ViewPath::parse(views, home, Path::new(other)),
                None,
                "{other}"
            );
        }
    }

    #[test]
    fn a_run_end_repair_records_the_homes_path_for_that_view_only() {
        let f = Fixture::new();
        let other_view = f.views.join("work/run-Live99");
        std::fs::create_dir_all(&other_view).unwrap();
        std::os::unix::fs::symlink(f.home.join("sessions"), other_view.join("sessions")).unwrap();
        f.row(ID, &f.through(&f.view, ID));
        f.row(OTHER, &f.through(&other_view, OTHER));
        assert_eq!(repair(&f.home, &f.views, Scope::View(&f.view)).unwrap(), 1);
        assert_eq!(f.path_of(ID), f.through(&f.home, ID));
        // Another live view's row is left to that run.
        assert_eq!(f.path_of(OTHER), f.through(&other_view, OTHER));
        // Repeating changes nothing.
        assert_eq!(repair(&f.home, &f.views, Scope::View(&f.view)).unwrap(), 0);
    }

    #[test]
    fn a_dangling_row_is_repaired_only_when_its_transcript_matches_canonically() {
        let f = Fixture::new();
        let gone = f.views.join("work/run-Gone00");
        f.row(ID, &f.through(&gone, ID));
        // The row's ID is not the one the file name carries.
        let mismatched = gone.join("sessions/2026/11/19").join(Fixture::name(ID));
        f.row(OTHER, &mismatched);
        let third = "01a1f204-e0de-7bc1-af6d-0d7a6f94155c";
        f.row(third, &f.through(&gone, third)); // no canonical copy
        let findings = scan(&f.home, &f.views).unwrap();
        assert_eq!(findings.len(), 3);
        assert!(findings.iter().all(|x| x.problem == Problem::Dangling));
        let canonical = |id: &str| {
            findings
                .iter()
                .find(|x| x.id.as_str() == id)
                .unwrap()
                .canonical
                .clone()
        };
        assert_eq!(canonical(ID), Some(f.through(&f.home, ID)));
        assert_eq!(canonical(OTHER), None);
        assert_eq!(canonical(third), None);
        // The scan changed nothing.
        assert_eq!(f.path_of(ID), f.through(&gone, ID));

        assert_eq!(repair(&f.home, &f.views, Scope::Gone).unwrap(), 1);
        assert_eq!(f.path_of(ID), f.through(&f.home, ID));
        assert_eq!(f.path_of(OTHER), mismatched);
        assert_eq!(f.path_of(third), f.through(&gone, third));
        // A live view is never "gone".
        f.row(ID, &f.through(&f.view, ID));
        assert_eq!(repair(&f.home, &f.views, Scope::Gone).unwrap(), 0);
        let live = scan(&f.home, &f.views).unwrap();
        assert!(
            live.iter()
                .any(|x| x.id.as_str() == ID && x.problem == Problem::InView)
        );
    }

    #[test]
    fn the_sweep_stands_aside_for_a_running_gc() {
        let f = Fixture::new();
        let gone = f.views.join("work/run-Gone00");
        f.row(ID, &f.through(&gone, ID));
        // gc renamed the view while retiring it: not gone yet.
        std::fs::create_dir_all(f.views.join("work/.retiring-run-Gone00")).unwrap();
        assert_eq!(repair(&f.home, &f.views, Scope::Gone).unwrap(), 0);
        std::fs::remove_dir(f.views.join("work/.retiring-run-Gone00")).unwrap();
        // gc holds the runtimes lock.
        let runtimes = std::fs::File::open(f.views.parent().unwrap()).unwrap();
        fs2::FileExt::lock_exclusive(&runtimes).unwrap();
        assert_eq!(repair(&f.home, &f.views, Scope::Gone).unwrap(), 0);
        fs2::FileExt::unlock(&runtimes).unwrap();
        assert_eq!(repair(&f.home, &f.views, Scope::Gone).unwrap(), 1);
        assert_eq!(f.path_of(ID), f.through(&f.home, ID));
    }

    #[test]
    fn a_view_whose_link_names_another_file_is_not_rewritten() {
        let f = Fixture::new();
        // The view's sessions is a private directory, not the home's.
        let private = f.views.join("work/run-Priv00");
        let day = private.join("sessions/2026/11/19");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::write(day.join(Fixture::name(ID)), "{\"private\":1}\n").unwrap();
        f.row(ID, &f.through(&private, ID));
        assert_eq!(repair(&f.home, &f.views, Scope::View(&private)).unwrap(), 0);
        assert_eq!(f.path_of(ID), f.through(&private, ID));
    }

    #[test]
    fn a_row_spelled_through_the_resolved_store_is_repaired_too() {
        // Codex may canonicalize CODEX_HOME: with a symlink above the store,
        // rows carry the resolved prefix, not aswap's spelling.
        let f = Fixture::new();
        let link = f._temp.path().join("linked-data");
        std::os::unix::fs::symlink(f._temp.path().join("aswap"), &link).unwrap();
        let views = link.join("runtimes/codex");
        let view = views.join("work/run-AbC123");
        f.row(ID, &f.through(&f.view, ID)); // the resolved spelling
        assert_eq!(repair(&f.home, &views, Scope::View(&view)).unwrap(), 1);
        assert_eq!(f.path_of(ID), f.through(&f.home, ID));
        let gone = f.views.join("work/run-Gone00");
        f.row(OTHER, &f.through(&gone, OTHER));
        assert_eq!(
            scan(&f.home, &views).unwrap()[0].canonical,
            Some(f.through(&f.home, OTHER))
        );
        assert_eq!(repair(&f.home, &views, Scope::Gone).unwrap(), 1);
    }

    #[test]
    fn malformed_rows_are_skipped_not_fatal() {
        let f = Fixture::new();
        let db = Connection::open(f.home.join("state_5.sqlite")).unwrap();
        // A table without NOT NULL, as another Codex version might have.
        db.execute_batch("DROP TABLE threads; CREATE TABLE threads (id TEXT, rollout_path TEXT);")
            .unwrap();
        db.execute("INSERT INTO threads VALUES (?1, NULL)", [OTHER])
            .unwrap();
        db.execute(
            "INSERT INTO threads VALUES (?1, ?2)",
            params!["01a1f204-02e0-7023-b4d3-12163ecf38ed", vec![0xffu8, 0xfe]],
        )
        .unwrap();
        db.execute(
            "INSERT INTO threads VALUES (?1, 'relative/path.jsonl')",
            [OTHER],
        )
        .unwrap();
        drop(db);
        let gone = f.views.join("work/run-Gone00");
        f.row(ID, &f.through(&gone, ID));
        let findings = scan(&f.home, &f.views).unwrap();
        assert_eq!(findings.len(), 1);
        assert_eq!(repair(&f.home, &f.views, Scope::Gone).unwrap(), 1);
        assert_eq!(f.path_of(ID), f.through(&f.home, ID));
    }

    #[test]
    fn a_home_without_a_thread_index_is_fine() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("codex");
        assert!(scan(&home, temp.path()).unwrap().is_empty());
        std::fs::create_dir_all(&home).unwrap();
        Connection::open(home.join("state_9.sqlite"))
            .unwrap()
            .execute_batch("CREATE TABLE other (x TEXT);")
            .unwrap();
        assert!(scan(&home, temp.path()).unwrap().is_empty());
        assert_eq!(repair(&home, temp.path(), Scope::Gone).unwrap(), 0);
    }
}
