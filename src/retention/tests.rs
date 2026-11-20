use super::*;
use std::os::unix::fs::{PermissionsExt, symlink};

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data/aswap");
        let home = temp.path().join("provider");
        fs::create_dir_all(&home).unwrap();
        Self {
            _temp: temp,
            root,
            home,
        }
    }

    fn view(&self, provider: &str, run: &str) -> PathBuf {
        let path = self
            .root
            .join("runtimes")
            .join(provider)
            .join("synthetic")
            .join(run);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn run(&self, mode: Mode) -> Report {
        self.run_with(
            mode,
            Duration::from_secs(3 * 86400),
            &mut |_| Ok(false),
            &mut || Ok(0.0),
        )
    }

    fn run_with(
        &self,
        mode: Mode,
        advance: Duration,
        in_use: &mut dyn FnMut(&Path) -> Result<bool>,
        pressure: &mut dyn FnMut() -> Result<f64>,
    ) -> Report {
        let settings = Settings {
            enabled: true,
            ..Settings::default()
        };
        Retention {
            root: &self.root,
            at: SystemTime::now() + advance,
            home: &|_| Ok(self.home.clone()),
            in_use,
            pressure: Pressure {
                read: pressure,
                limit: settings.io_pressure_limit,
                next: Instant::now(),
                stopped: None,
            },
        }
        .run(settings, mode)
        .unwrap()
    }
}

fn write(path: &Path, data: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, data).unwrap();
}

#[test]
fn preview_and_apply_account_for_duplicates_moves_and_regenerable_entries() {
    let t = Fixture::new();
    let v = t.view("codex", "run-one");
    let duplicate = "sessions/2026/10/12/rollout-duplicate.jsonl";
    let unique = "sessions/2026/10/12/rollout-unique.jsonl";
    let archived = "archived_sessions/rollout-old.jsonl";
    write(&v.join(duplicate), b"duplicate");
    write(&t.home.join(duplicate), b"duplicate");
    write(&v.join(unique), b"unique");
    write(&v.join(archived), b"old");
    write(&v.join("tmp/output"), b"cache");
    write(&v.join("logs_2.sqlite"), b"database");
    write(&v.join("auth.json"), b"synthetic-private");
    let saved = t._temp.path().join("external");
    write(&saved, b"keep this");
    symlink(&saved, v.join("external")).unwrap();

    let bytes_reclaimed = 9 + 5 + 8 + 17;
    let report = t.run(Mode::Configured);
    assert!(report.dry_run);
    assert_eq!(report.totals.finished, 1);
    assert_eq!(report.totals.removed, 0);
    assert_eq!(report.totals.bytes_reclaimed, bytes_reclaimed);
    assert_eq!(report.totals.bytes_moved, 9);
    assert_eq!(report.totals.bytes_archived, 0);
    assert!(v.join(duplicate).is_file());
    assert!(v.join(unique).is_file());
    assert!(!t.home.join(unique).exists());
    assert!(!shared::view_lock_path(&v).exists());
    assert!(!t.root.join("archive").exists());
    assert!(
        report
            .text()
            .contains(&format!("{bytes_reclaimed} bytes to reclaim"))
    );
    assert!(!report.text().contains("synthetic-private"));

    let report = t.run(Mode::Apply);
    assert_eq!(report.totals.examined, 1);
    assert_eq!(report.totals.removed, 1);
    assert_eq!(report.totals.bytes_reclaimed, bytes_reclaimed);
    assert_eq!(report.totals.bytes_moved, 9);
    assert_eq!(fs::read(t.home.join(duplicate)).unwrap(), b"duplicate");
    assert_eq!(fs::read(t.home.join(unique)).unwrap(), b"unique");
    assert_eq!(fs::read(t.home.join(archived)).unwrap(), b"old");
    assert!(
        fs::metadata(t.home.join(unique))
            .unwrap()
            .permissions()
            .mode()
            & 0o077
            == 0
    );
    assert!(!v.exists());
    assert_eq!(fs::read(saved).unwrap(), b"keep this");
    assert!(!t.home.join("auth.json").exists());
}

#[test]
fn claude_conflicts_are_zstd_archived_and_unique_transcripts_are_resumable() {
    let t = Fixture::new();
    let v = t.view("claude", "run-conflict");
    let same_size = "projects/synthetic-project/synthetic-session.jsonl";
    let different_size = "projects/synthetic-project/different-session.jsonl";
    let unique = "projects/synthetic-project/unique-session.jsonl";
    let original = vec![b'a'; 4096];
    let alternate = vec![b'b'; 4096];
    write(&v.join(same_size), &original);
    write(&t.home.join(same_size), &alternate);
    write(&v.join(different_size), b"from view");
    write(&t.home.join(different_size), b"shared");
    write(&v.join(unique), b"claude session");
    write(&v.join("log/first"), b"log");

    let preview = t.run(Mode::Preview);
    assert_eq!(preview.totals.bytes_reclaimed, 3);
    assert_eq!(preview.totals.bytes_moved, 14);
    assert_eq!(preview.totals.bytes_archived, 4096 + 9);
    let apply = t.run(Mode::Apply);
    assert_eq!(apply.totals.bytes_archived, 4096 + 9);
    assert_eq!(apply.totals.removed, 1);
    assert!(!v.exists());
    assert_eq!(fs::read(t.home.join(same_size)).unwrap(), alternate);
    assert_eq!(fs::read(t.home.join(different_size)).unwrap(), b"shared");
    assert_eq!(fs::read(t.home.join(unique)).unwrap(), b"claude session");
    let archive = t.root.join("archive/claude/synthetic/run-conflict");
    let mut stored = 0;
    let mut expected_manifest = String::new();
    for (name, transcript) in [
        (different_size, b"from view".as_slice()),
        (same_size, &original),
    ] {
        let compressed = fs::read(archive.join(format!("{name}.zst"))).unwrap();
        stored += compressed.len() as u64;
        let hash = ring::digest::digest(&ring::digest::SHA256, &compressed);
        let hex: String = hash.as_ref().iter().map(|b| format!("{b:02x}")).collect();
        writeln!(expected_manifest, "{hex}  {name}.zst").unwrap();
        assert_eq!(
            zstd::stream::decode_all(&compressed[..]).unwrap(),
            transcript
        );
    }
    let manifest = fs::read_to_string(archive.join("SHA256SUMS")).unwrap();
    assert_eq!(manifest, expected_manifest);
    assert_eq!(
        apply.totals.bytes_reclaimed,
        4096 + 9 + 3 - i128::from(stored) - manifest.len() as i128
    );
    assert_eq!(
        apply.groups[0].views[0].archive.as_deref(),
        archive.to_str()
    );
}

#[test]
fn locked_young_live_or_uncertain_views_are_kept() {
    let t = Fixture::new();
    let locked = t.view("codex", "run-locked");
    let sibling = File::create(shared::view_lock_path(&locked)).unwrap();
    fs2::FileExt::lock_exclusive(&sibling).unwrap();
    let active = t.view("codex", "run-active");
    let lifetime = File::create(active.join(shared::VIEW_LIFETIME_LOCK)).unwrap();
    fs2::FileExt::lock_exclusive(&lifetime).unwrap();
    let live_view = t.view("codex", "run-live");
    let uncertain = t.view("codex", "run-uncertain");
    let finished = t.view("codex", "run-finished");

    let young = t.run_with(Mode::Apply, Duration::ZERO, &mut |_| Ok(false), &mut || {
        Ok(0.0)
    });
    assert_eq!(young.totals.kept, 5);
    assert_eq!(young.groups[0].views[0].reason, "younger than minimum age");

    let report = t.run_with(
        Mode::Apply,
        Duration::from_secs(3 * 86400),
        &mut |path| {
            if path == uncertain {
                return Err(Error("cannot establish process liveness"));
            }
            Ok(path == live_view)
        },
        &mut || Ok(30.0),
    );
    assert_eq!(report.totals.removed, 1);
    assert_eq!(report.totals.kept, 4);
    assert!(report.text().contains("view lock held"));
    assert!(report.text().contains("used by a live process"));
    assert!(report.text().contains("cannot establish process liveness"));
    for v in [locked, active, live_view, uncertain] {
        assert!(v.is_dir());
    }
    assert!(!finished.exists());
}

#[test]
fn process_scan_checks_both_provider_variables_and_descendant_cwd() {
    let t = Fixture::new();
    let v = t.view("codex", "run-live");
    let proc = t._temp.path().join("proc");
    let pid = proc.join("123");
    write(
        &pid.join("status"),
        b"Name:\tsynthetic\nState:\tS\nUid:\t1234\t1234\t1234\t1234\n",
    );
    symlink(&t.home, pid.join("cwd")).unwrap();
    for var in ["CODEX_HOME", "CLAUDE_CONFIG_DIR"] {
        write(
            &pid.join("environ"),
            format!("EXAMPLE=synthetic\0{var}={}\0", v.display()).as_bytes(),
        );
        assert!(process::scan(&proc, 1234, &v).unwrap());
        assert!(!process::scan(&proc, 1235, &v).unwrap());
    }
    write(
        &pid.join("environ"),
        b"CODEX_HOME=/synthetic-path-that-does-not-exist\0",
    );
    assert!(!process::scan(&proc, 1234, &v).unwrap());
    fs::remove_file(pid.join("cwd")).unwrap();
    assert!(process::scan(&proc, 1234, &v).is_err());
    symlink(v.join("tmp"), pid.join("cwd")).unwrap();
    assert!(process::scan(&proc, 1234, &v).unwrap());
}

#[test]
fn symlinked_view_and_unmanaged_transcript_links_are_kept() {
    let t = Fixture::new();
    let v = t.view("codex", "run-symlink");
    let local = "sessions/a/rollout-synthetic.jsonl";
    let external = t._temp.path().join("external");
    write(&external.join("a/rollout-synthetic.jsonl"), b"untouched");
    fs::create_dir_all(v.join("sessions/a")).unwrap();
    symlink(external.join("a/rollout-synthetic.jsonl"), v.join(local)).unwrap();
    symlink(&external, v.with_file_name("run-foreign")).unwrap();
    let report = t.run(Mode::Apply);
    assert_eq!(report.totals.kept, 2);
    assert_eq!(
        fs::read(external.join("a/rollout-synthetic.jsonl")).unwrap(),
        b"untouched"
    );

    fs::remove_file(v.join(local)).unwrap();
    write(&v.join(local), b"private transcript");
    write(&t.home.join("sessions/placeholder"), b"synthetic");
    symlink(external.join("a"), t.home.join("sessions/a")).unwrap();
    let report = t.run(Mode::Apply);
    assert_eq!(report.totals.kept, 2);
    assert!(v.join(local).is_file());
    assert_eq!(
        fs::read(external.join("a/rollout-synthetic.jsonl")).unwrap(),
        b"untouched"
    );
}

#[test]
fn generated_shared_transcript_link_is_unlinked_without_touching_target() {
    let t = Fixture::new();
    let v = t.view("claude", "run-symlink");
    write(
        &t.home.join("projects/synthetic/synthetic.jsonl"),
        b"shared",
    );
    symlink(t.home.join("projects"), v.join("projects")).unwrap();
    let report = t.run(Mode::Apply);
    assert_eq!(report.totals.removed, 1);
    assert_eq!(report.totals.bytes_reclaimed, 0);
    assert!(t.home.join("projects/synthetic/synthetic.jsonl").exists());
}

#[test]
fn pressure_stops_before_next_view_and_mid_view_publication_can_be_retried() {
    let t = Fixture::new();
    let a = t.view("codex", "run-a");
    let b = t.view("codex", "run-b");
    let path = "sessions/rollout-synthetic.jsonl";
    write(&a.join(path), b"synthetic transcript");
    let after_publish = t.run_with(
        Mode::Apply,
        Duration::from_secs(3 * 86400),
        &mut |_| Ok(false),
        &mut || {
            Ok(if t.home.join(path).is_file() {
                31.0
            } else {
                30.0
            })
        },
    );
    assert!(after_publish.stopped.is_some());
    assert_eq!(after_publish.totals.examined, 1);
    assert_eq!(after_publish.totals.removed, 0);
    assert_eq!(after_publish.totals.bytes_moved, 20);
    assert_eq!(after_publish.totals.bytes_reclaimed, -20);
    assert!(a.join(path).exists());
    assert_eq!(
        fs::read(t.home.join(path)).unwrap(),
        b"synthetic transcript"
    );
    assert!(b.exists());

    let mut reads = 0;
    let between_views = t.run_with(
        Mode::Apply,
        Duration::from_secs(3 * 86400),
        &mut |_| Ok(false),
        &mut || {
            reads += 1;
            Ok(if reads >= 3 { 31.0 } else { 0.0 })
        },
    );
    assert!(between_views.stopped.is_some());
    assert_eq!(between_views.totals.removed, 1);
    assert_eq!(between_views.totals.bytes_moved, 0);
    assert!(!a.exists());
    assert!(b.exists());
    assert!(t.run(Mode::Apply).stopped.is_none());
    assert!(!b.exists());
}

#[test]
fn existing_archive_is_verified_on_retry_and_corruption_keeps_the_view() {
    let t = Fixture::new();
    let view = t.view("claude", "run-retry");
    let archive = t.root.join("archive/claude/synthetic/run-retry");
    let path = "projects/synthetic/synthetic.jsonl";
    write(&view.join(path), b"from view");
    write(&t.home.join(path), b"from shared");
    let report = t.run_with(
        Mode::Apply,
        Duration::from_secs(3 * 86400),
        &mut |p| Ok(p == view && archive.exists()),
        &mut || Ok(0.0),
    );
    assert_eq!(report.totals.removed, 0);
    assert!(view.join(path).is_file());
    assert_eq!(report.totals.bytes_archived, 9);
    let manifest = fs::read(archive.join("SHA256SUMS")).unwrap();
    fs::write(archive.join("SHA256SUMS"), b"malformed manifest").unwrap();
    assert_eq!(t.run(Mode::Apply).totals.kept, 1);
    assert!(view.join(path).is_file());
    fs::write(archive.join("SHA256SUMS"), manifest).unwrap();
    assert_eq!(t.run(Mode::Apply).totals.removed, 1);
    assert!(!view.exists());
}

#[test]
fn pressure_parser_uses_some_avg10_and_rejects_invalid_input() {
    assert_eq!(
        process::parse_pressure(
            "some avg10=31.25 avg60=0.10 avg300=0.00 total=25\nfull avg10=1.50 total=15"
        )
        .unwrap(),
        31.25
    );
    assert!(process::parse_pressure("full avg10=1.50 total=15").is_err());
    assert!(process::parse_pressure("some avg10=NaN").is_err());
}

const CODEX_ID: &str = "00000000-1111-2222-3333-444444444444";
const SECOND_ID: &str = "00000000-1111-2222-3333-555555555555";

fn codex_index(t: &Fixture, rows: &[(&str, &Path)]) -> PathBuf {
    let path = t.home.join("state_5.sqlite");
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL);").unwrap();
    for (id, rollout) in rows {
        db.execute(
            "INSERT INTO threads VALUES (?1, ?2)",
            rusqlite::params![id, rollout.to_str().unwrap()],
        )
        .unwrap();
    }
    path
}

fn indexed_rollout(db: &Path, id: &str) -> PathBuf {
    let db = rusqlite::Connection::open(db).unwrap();
    let path: String = db
        .query_row(
            "SELECT rollout_path FROM threads WHERE id = ?1",
            [id],
            |row| row.get(0),
        )
        .unwrap();
    path.into()
}

#[test]
fn codex_resume_index_is_rewritten_after_publication_and_backed_up_once() {
    let t = Fixture::new();
    let copied = t.view("codex", "run-a");
    let linked = t.view("codex", "run-b");
    let first = format!("sessions/2026/10/12/rollout-synthetic-{CODEX_ID}.jsonl");
    let second = format!("sessions/2026/10/12/rollout-synthetic-{SECOND_ID}.jsonl");
    write(&copied.join(&first), b"unique rollout");
    write(&t.home.join(&second), b"shared rollout");
    symlink(t.home.join("sessions"), linked.join("sessions")).unwrap();
    let db = codex_index(
        &t,
        &[
            (CODEX_ID, &copied.join(&first)),
            (SECOND_ID, &linked.join(&second)),
        ],
    );

    let preview = t.run(Mode::Preview);
    assert_eq!(preview.totals.finished, 2, "{}", preview.text());
    assert!(!t.root.join("archive").exists());
    assert!(!t.home.join("state_5.sqlite-shm").exists());
    assert!(!t.home.join("state_5.sqlite-wal").exists());

    let applied = t.run(Mode::Apply);
    assert_eq!(applied.totals.removed, 2, "{}", applied.text());
    let views = &applied.groups[0].views;
    assert_eq!(views[0].codex_index_rows_rewritten, 1);
    assert_eq!(views[1].codex_index_rows_rewritten, 1);
    assert_eq!(views[0].codex_index_backups.len(), 1);
    assert!(views[1].codex_index_backups.is_empty());
    assert_eq!(indexed_rollout(&db, CODEX_ID), t.home.join(&first));
    assert_eq!(indexed_rollout(&db, SECOND_ID), t.home.join(&second));
    let backup = Path::new(&views[0].codex_index_backups[0]);
    assert_eq!(indexed_rollout(backup, CODEX_ID), copied.join(&first));
    assert_eq!(indexed_rollout(backup, SECOND_ID), linked.join(&second));
    assert_eq!(fs::read(t.home.join(&first)).unwrap(), b"unique rollout");
    assert_eq!(fs::read(t.home.join(&second)).unwrap(), b"shared rollout");
}

#[test]
fn malformed_or_conflicting_codex_index_keeps_view_and_original_row() {
    let t = Fixture::new();
    let conflict = t.view("codex", "run-conflict");
    let malformed = t.view("codex", "run-malformed");
    let first = format!("sessions/rollout-synthetic-{CODEX_ID}.jsonl");
    let second = format!("sessions/rollout-synthetic-{SECOND_ID}.jsonl");
    write(&conflict.join(&first), b"source rollout");
    write(&t.home.join(&first), b"shared rollout");
    write(&malformed.join(&second), b"another rollout");
    let db = codex_index(
        &t,
        &[
            (CODEX_ID, &conflict.join(&first)),
            ("malformed", &malformed.join(&second)),
        ],
    );

    for mode in [Mode::Preview, Mode::Apply] {
        let report = t.run(mode);
        assert_eq!(report.totals.kept, 2, "{}", report.text());
        assert!(report.text().contains("cannot be safely mapped"));
        assert!(report.text().contains("malformed Codex index row"));
        assert_eq!(indexed_rollout(&db, CODEX_ID), conflict.join(&first));
        assert_eq!(indexed_rollout(&db, "malformed"), malformed.join(&second));
        assert!(conflict.join(&first).exists());
        assert!(malformed.join(&second).exists());
    }
    assert!(!t.root.join("archive/codex/index-backups").exists());
}
