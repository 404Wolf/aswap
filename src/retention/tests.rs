use super::*;
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
};

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
        self.run_with_settings(
            mode,
            advance,
            in_use,
            pressure,
            &mut || Ok(mounts::Mounts::default()),
            self.settings(),
        )
    }

    fn settings(&self) -> Settings {
        Settings {
            enabled: true,
            legacy_homes: ["claude:synthetic", "codex:synthetic"]
                .into_iter()
                .map(|label| (label.into(), self.home.clone()))
                .collect(),
            ..Settings::default()
        }
    }

    fn run_with_settings(
        &self,
        mode: Mode,
        advance: Duration,
        in_use: &mut dyn FnMut(&Path) -> Result<bool>,
        pressure: &mut dyn FnMut() -> Result<f64>,
        mounts: &mut dyn FnMut() -> Result<mounts::Mounts>,
        settings: Settings,
    ) -> Report {
        Retention {
            root: &self.root,
            at: SystemTime::now() + advance,
            in_use,
            recent_fds: &mut |_, _| Ok(false),
            liveness_error: &Cell::new(None),
            mounts,
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
    write(&t.home.join("auth.json"), b"synthetic-private");
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
            .contains(&format!("{bytes_reclaimed} logical bytes to reclaim"))
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
    assert_eq!(
        fs::read(t.home.join("auth.json")).unwrap(),
        b"synthetic-private"
    );
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
    let mut ignored = Default::default();
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
        assert!(process::scan(&proc, 1234, &v, &mut ignored).unwrap());
        assert!(!process::scan(&proc, 1235, &v, &mut ignored).unwrap());
    }
    write(
        &pid.join("environ"),
        b"CODEX_HOME=/synthetic-path-that-does-not-exist\0",
    );
    assert!(!process::scan(&proc, 1234, &v, &mut ignored).unwrap());
    fs::remove_file(pid.join("cwd")).unwrap();
    assert!(process::scan(&proc, 1234, &v, &mut ignored).is_err());
    symlink(v.join("tmp"), pid.join("cwd")).unwrap();
    assert!(process::scan(&proc, 1234, &v, &mut ignored).unwrap());
    assert!(ignored.is_empty());
}

#[test]
fn unreadable_processes_use_cmdline_to_distinguish_non_providers_from_ambiguity() {
    use std::io;
    use std::os::unix::ffi::OsStrExt;

    let t = Fixture::new();
    let v = Path::new("/synthetic-runtimes/run-example");
    let proc = t._temp.path().join("proc");
    let pid = proc.join("123");
    write(
        &pid.join("status"),
        b"State:\tS\nUid:\t1234\t1234\t1234\t1234\n",
    );
    symlink(&t.home, pid.join("cwd")).unwrap();
    let cmdline = pid.join("cmdline");
    let scan = |deny_cwd, ignored: &mut _| {
        process::scan_with_access(
            &proc,
            1234,
            v,
            ignored,
            |path| {
                if deny_cwd {
                    Err(io::ErrorKind::PermissionDenied.into())
                } else {
                    fs::read_link(path)
                }
            },
            |_| Err(io::ErrorKind::PermissionDenied.into()),
        )
    };

    let mut ignored = Default::default();
    write(&cmdline, b"sshd-session\0synthetic\0");
    assert!(!scan(true, &mut ignored).unwrap());
    assert!(!scan(false, &mut ignored).unwrap());
    assert_eq!(ignored.len(), 1, "repeated scans count one PID once");

    let mut view_cmdline = b"shell\0".to_vec();
    view_cmdline.extend_from_slice(v.as_os_str().as_bytes());
    for command in [
        b"".as_slice(),
        b"\0",
        b"Claude\0",
        b"codex\0",
        b"aswap\0",
        b"shell\0/usr/bin/node\0",
        &view_cmdline,
    ] {
        write(&cmdline, command);
        let mut ignored = Default::default();
        for deny_cwd in [true, false] {
            let failure = scan(deny_cwd, &mut ignored).unwrap_err();
            let field = if deny_cwd { "cwd" } else { "environ" };
            let diagnostic = failure.blocker.unwrap().to_string();
            assert!(diagnostic.contains(&format!("{field} PermissionDenied")));
        }
        assert!(ignored.is_empty());
    }
    fs::remove_file(cmdline).unwrap();
    assert!(scan(true, &mut Default::default()).is_err());
}

#[test]
fn process_scan_rechecks_a_transient_cwd_error_and_an_exiting_process() {
    let t = Fixture::new();
    let view = t.view("codex", "run-cwd");
    let proc = t._temp.path().join("proc");
    let pid = proc.join("321");
    let status = pid.join("status");
    write(&status, b"State:\tS\nUid:\t1234\t1234\t1234\t1234\n");
    write(&pid.join("cmdline"), b"");
    let attempts = Cell::new(0);
    assert!(
        process::scan_with_access(
            &proc,
            1234,
            &view,
            &mut Default::default(),
            |_| {
                attempts.set(attempts.get() + 1);
                if attempts.get() == 1 {
                    Err(ErrorKind::PermissionDenied.into())
                } else {
                    Ok(view.join("descendant"))
                }
            },
            |_| unreachable!("cwd already matches"),
        )
        .unwrap()
    );
    assert_eq!(attempts.get(), 2);
    assert!(
        !process::scan_with_access(
            &proc,
            1234,
            &view,
            &mut Default::default(),
            |_| Ok(t.home.clone()),
            |_| {
                write(&status, b"State:\tZ\nUid:\t1234\t1234\t1234\t1234\n");
                Err(ErrorKind::NotFound.into())
            },
        )
        .unwrap()
    );
}

#[test]
fn late_environ_blocker_names_pid_and_preserves_archive_for_retry() {
    let t = Fixture::new();
    let view = t.view("codex", "run-environ");
    let archive = t.root.join("archive/codex/synthetic/run-environ");
    let proc = t._temp.path().join("proc");
    let pid = proc.join("322");
    write(
        &pid.join("status"),
        b"State:\tS\nUid:\t1234\t1234\t1234\t1234\n",
    );
    write(
        &pid.join("cmdline"),
        b"/bin/codex\0synthetic-secret-argument\0",
    );
    write(&view.join("history.jsonl"), b"synthetic history");
    let diagnostic = Cell::new(None);
    let report = Retention {
        root: &t.root,
        at: SystemTime::now() + Duration::from_secs(3 * 86400),
        in_use: &mut |path| {
            if !archive.exists() {
                return Ok(false);
            }
            record_liveness(
                process::scan_with_access(
                    &proc,
                    1234,
                    path,
                    &mut Default::default(),
                    |_| Ok(t.home.clone()),
                    |_| Err(ErrorKind::PermissionDenied.into()),
                ),
                &diagnostic,
            )
        },
        liveness_error: &diagnostic,
        recent_fds: &mut |_, _| Ok(false),
        mounts: &mut || Ok(mounts::Mounts::default()),
        pressure: Pressure {
            read: &mut || Ok(0.0),
            limit: 30.0,
            next: Instant::now(),
            stopped: None,
        },
    }
    .run(t.settings(), Mode::Apply)
    .unwrap();
    let text = report.text();
    assert!(text.contains("cannot establish process liveness (PID 322, cmdline excerpt codex"));
    assert!(text.contains("environ PermissionDenied"));
    assert!(!text.contains("synthetic-secret-argument"));
    assert!(text.contains("Archive retained for retry"));
    assert_eq!(report.totals.kept, 1);
    assert!(view.join("history.jsonl").is_file());
    let manifest = fs::read(archive.join("SHA256SUMS")).unwrap();
    let retry = t.run(Mode::Apply);
    assert_eq!(retry.totals.removed, 1, "{}", retry.text());
    assert_eq!(fs::read(archive.join("SHA256SUMS")).unwrap(), manifest);
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

#[test]
fn private_files_and_committed_wal_are_restorable_from_owner_only_archive() {
    let t = Fixture::new();
    let view = t.view("claude", "run-private");
    let history = b"synthetic history";
    let credential = b"synthetic private credential";
    write(&view.join("history.jsonl"), history);
    write(&t.home.join("history.jsonl"), b"another history");
    write(&view.join(".credentials.json"), credential);
    write(&view.join("settings.json"), b"shared");
    write(&t.home.join("settings.json"), b"shared");
    write(&view.join("models_cache.json"), b"discard");
    write(&view.join("logs_2.sqlite-wal"), b"discard");
    let db = rusqlite::Connection::open(view.join("state_5.sqlite")).unwrap();
    db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE local_data (value TEXT); INSERT INTO local_data VALUES ('synthetic WAL');").unwrap();
    assert!(view.join("state_5.sqlite-wal").exists());

    let report = t.run(Mode::Apply);
    assert_eq!(report.totals.removed, 1, "{}", report.text());
    assert!(!report.text().contains("synthetic private credential"));
    let archive = t.root.join("archive/claude/synthetic/run-private");
    assert_eq!(
        archive.metadata().unwrap().permissions().mode() & 0o777,
        0o700
    );
    let restore = |name: &str| {
        let path = archive.join(format!("{name}.zst"));
        assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        zstd::stream::decode_all(fs::File::open(path).unwrap()).unwrap()
    };
    assert_eq!(restore("history.jsonl"), history);
    assert_eq!(restore(".credentials.json"), credential);
    assert_eq!(
        fs::read(t.home.join("history.jsonl")).unwrap(),
        b"another history"
    );
    assert!(!t.home.join(".credentials.json").exists());
    for name in [
        "state_5.sqlite-wal",
        "state_5.sqlite-shm",
        "settings.json",
        "models_cache.json",
        "logs_2.sqlite-wal",
    ] {
        assert!(!archive.join(format!("{name}.zst")).exists());
    }
    let restored = t._temp.path().join("restored.sqlite");
    write(&restored, &restore("state_5.sqlite"));
    let restored = rusqlite::Connection::open(&restored).unwrap();
    let value: String = restored
        .query_row("SELECT value FROM local_data", [], |row| row.get(0))
        .unwrap();
    assert_eq!(value, "synthetic WAL");
}

#[test]
fn unmarked_legacy_views_require_an_explicit_label_mapping() {
    let t = Fixture::new();
    let view = t.view("codex", "run-legacy");
    let other = view
        .parent()
        .unwrap()
        .with_file_name("different")
        .join("run-legacy");
    let relative = "sessions/rollout-synthetic.jsonl";
    write(&view.join(relative), b"synthetic");
    write(&other.join(relative), b"unmapped");
    let mut settings = t.settings();
    settings.legacy_homes.clear();
    let run = |settings| {
        t.run_with_settings(
            Mode::Apply,
            Duration::from_secs(3 * 86400),
            &mut |_| Ok(false),
            &mut || Ok(0.0),
            &mut || Ok(mounts::Mounts::default()),
            settings,
        )
    };
    let preview = run(settings);
    assert_eq!(preview.totals.kept, 2);
    assert!(
        preview
            .text()
            .contains("legacy view without a verified home")
    );
    assert!(!t.home.join(relative).exists());

    let mapped = t._temp.path().join("legacy-home");
    fs::create_dir(&mapped).unwrap();
    let mut settings = Settings {
        enabled: true,
        ..Settings::default()
    };
    settings
        .set_legacy_homes(&[format!("codex:synthetic={}", mapped.display())])
        .unwrap();
    let applied = run(settings);
    assert_eq!(applied.totals.removed, 1, "{}", applied.text());
    assert_eq!(fs::read(mapped.join(relative)).unwrap(), b"synthetic");
    assert!(other.join(relative).exists());
    assert!(!t.home.join(relative).exists());
}

#[test]
fn alternate_codex_index_spellings_are_repaired_or_reported_as_uncertain() {
    let t = Fixture::new();
    let view = t.view("codex", "run-spelling");
    let uncertain = t.view("codex", "run-unresolved");
    let first = format!("sessions/rollout-synthetic-{CODEX_ID}.jsonl");
    let second = format!("sessions/rollout-synthetic-{SECOND_ID}.jsonl");
    write(&view.join(&first), b"synthetic");
    write(&uncertain.join(&second), b"do not lose this");
    let stored = PathBuf::from(format!(
        "{}/.//run-spelling/{first}",
        view.parent().unwrap().display()
    ));
    let unresolved = t
        ._temp
        .path()
        .join("nonexistent/run-unresolved")
        .join(&second);
    let index = codex_index(&t, &[(CODEX_ID, &stored), (SECOND_ID, &unresolved)]);
    let applied = t.run(Mode::Apply);
    assert_eq!(applied.totals.removed, 1, "{}", applied.text());
    assert!(
        applied
            .text()
            .contains("cannot resolve possible Codex index reference")
    );
    assert_eq!(indexed_rollout(&index, CODEX_ID), t.home.join(&first));
    assert!(uncertain.join(&second).exists());
}

#[test]
fn index_row_inserted_during_retirement_restores_the_view() {
    let t = Fixture::new();
    let view = t.view("codex", "run-retirement");
    let relative = format!("sessions/rollout-synthetic-{CODEX_ID}.jsonl");
    write(&view.join(&relative), b"synthetic");
    let db = codex_index(&t, &[]);
    let tombstone = view.with_file_name(".retiring-run-retirement");
    let mut inserted = false;
    let applied = t.run_with(
        Mode::Apply,
        Duration::from_secs(3 * 86400),
        &mut |path| {
            if path == tombstone {
                assert!(!view.exists());
                assert!(tombstone.is_dir());
                let db = rusqlite::Connection::open(&db).unwrap();
                db.execute(
                    "INSERT INTO threads VALUES (?1, ?2)",
                    rusqlite::params![CODEX_ID, view.join(&relative).to_str()],
                )
                .unwrap();
                inserted = true;
            }
            Ok(false)
        },
        &mut || Ok(0.0),
    );
    assert!(inserted);
    assert!(applied.incomplete());
    assert!(
        applied
            .text()
            .contains("Codex index still references the retiring view")
    );
    assert_eq!(applied.totals.removed, 0);
    assert_eq!(fs::read(view.join(relative)).unwrap(), b"synthetic");
    assert!(!tombstone.exists());
}

#[test]
fn interrupted_retirement_is_restored_only_on_apply() {
    let t = Fixture::new();
    let view = t.view("codex", "run-interrupted");
    let tombstone = view.with_file_name(".retiring-run-interrupted");
    write(&view.join("sessions/test.jsonl"), b"synthetic");
    fs::rename(&view, &tombstone).unwrap();
    assert!(t.run(Mode::Preview).text().contains("needs recovery"));
    assert!(!view.exists());
    let report = t.run(Mode::Apply);
    assert!(report.incomplete());
    assert!(report.text().contains("restored interrupted retirement"));
    assert!(view.join("sessions/test.jsonl").exists());
    assert!(!tombstone.exists());
}

#[test]
fn descriptor_snapshot_and_nested_provider_home_keep_old_views() {
    let t = Fixture::new();
    let view = t.view("codex", "run-descriptor");
    let relative = "sessions/rollout-synthetic.jsonl";
    write(&view.join(relative), b"synthetic");
    let proc = t._temp.path().join("proc");
    let pid = proc.join("123");
    write(
        &pid.join("status"),
        b"State:\tS\nUid:\t1234\t1234\t1234\t1234\n",
    );
    fs::create_dir_all(pid.join("fd")).unwrap();
    symlink(view.join(relative), pid.join("fd/4")).unwrap();
    symlink(&t.home, pid.join("cwd")).unwrap();
    write(
        &pid.join("environ"),
        format!("CODEX_HOME={}\0", view.join("nested").display()).as_bytes(),
    );
    let mut ignored = Default::default();
    assert!(process::scan(&proc, 1234, &view, &mut ignored).unwrap());
    let fds = process::Fds::read(&proc, 1234, &[&t.root.join("runtimes")], None).unwrap();
    fs::remove_file(pid.join("fd/4")).unwrap();
    let report = t.run_with(
        Mode::Apply,
        Duration::from_secs(3 * 86400),
        &mut |path| fds.uses_view(path, &mut ignored).map_err(Error::from),
        &mut || Ok(0.0),
    );
    assert!(report.text().contains("used by a live process"));
    assert!(view.join(relative).exists());
    let fds = process::Fds::read(&proc, 1234, &[&t.root.join("runtimes")], None).unwrap();
    assert!(!fds.uses_view(&view, &mut ignored).unwrap());
}

fn synthetic_mount(path: &Path) -> Result<mounts::Mounts> {
    let escaped = path.to_string_lossy().replace(' ', "\\040");
    mounts::Mounts::parse(format!("42 1 0:7 / {escaped} rw - ext4 synthetic rw\n").as_bytes())
}

#[test]
fn mount_checks_include_run_root_descendants_and_new_mounts_before_deletion() {
    let t = Fixture::new();
    let view = t.view("codex", "run-mounted");
    write(&view.join("tmp/mount space/file"), b"synthetic");
    let tombstone = view.with_file_name(".retiring-run-mounted");
    let run = |mounts: &mut dyn FnMut() -> Result<mounts::Mounts>| {
        t.run_with_settings(
            Mode::Apply,
            Duration::from_secs(3 * 86400),
            &mut |_| Ok(false),
            &mut || Ok(0.0),
            mounts,
            t.settings(),
        )
    };
    for mount in [&view, &view.join("tmp/mount space")] {
        let report = run(&mut || synthetic_mount(mount));
        assert!(report.text().contains("view contains a mount point"));
        assert!(view.join("tmp/mount space/file").exists());
    }
    let report = run(&mut || {
        if tombstone.exists() {
            synthetic_mount(&tombstone.join("tmp/mount space"))
        } else {
            Ok(mounts::Mounts::default())
        }
    });
    assert!(report.incomplete());
    assert!(report.text().contains("view contains a mount point"));
    assert!(view.join("tmp/mount space/file").exists());
    assert!(!tombstone.exists());
}

#[test]
fn published_archive_identity_is_rechecked_and_retries_validate_surviving_subset() {
    let t = Fixture::new();
    let view = t.view("claude", "run-archive");
    let archive = t.root.join("archive/claude/synthetic/run-archive");
    let moved_archive = archive.with_file_name("previous");
    let tombstone = view.with_file_name(".retiring-run-archive");
    write(&view.join("unknown-a"), b"keep a");
    write(&view.join("unknown-b"), b"keep b");
    let report = t.run_with(
        Mode::Apply,
        Duration::from_secs(3 * 86400),
        &mut |path| {
            if path == tombstone {
                fs::rename(&archive, &moved_archive).unwrap();
                fs::create_dir(&archive).unwrap();
                fs::copy(moved_archive.join("SHA256SUMS"), archive.join("SHA256SUMS")).unwrap();
            }
            Ok(false)
        },
        &mut || Ok(0.0),
    );
    assert!(report.incomplete());
    assert!(
        report
            .text()
            .contains("published archive identity or manifest changed")
    );
    assert!(view.join("unknown-a").exists());
    fs::remove_file(archive.join("SHA256SUMS")).unwrap();
    fs::remove_dir(&archive).unwrap();
    fs::rename(&moved_archive, &archive).unwrap();
    fs::remove_file(view.join("unknown-a")).unwrap();
    let retried = t.run(Mode::Apply);
    assert_eq!(retried.totals.removed, 1, "{}", retried.text());
    assert!(archive.join("unknown-a.zst").exists());
    assert!(archive.join("unknown-b.zst").exists());
}

#[test]
fn pressure_stop_during_bounded_deletion_restores_partial_view_and_reports_incomplete() {
    let t = Fixture::new();
    let view = t.view("claude", "run-delete");
    for index in 0..80 {
        write(&view.join(format!("tmp/{index:02}")), b"X");
    }
    let mut samples = 0;
    let report = t.run_with(
        Mode::Apply,
        Duration::from_secs(3 * 86400),
        &mut |_| Ok(false),
        &mut || {
            samples += 1;
            Ok(if samples >= 3 { 31.0 } else { 0.0 })
        },
    );
    assert!(report.incomplete());
    assert!(report.stopped.is_some());
    assert_eq!(report.totals.removed, 0);
    assert!(view.exists());
    assert!(report.totals.bytes_reclaimed > 0 && report.totals.bytes_reclaimed < 80);
    assert_eq!(t.run(Mode::Apply).totals.removed, 1);
}

#[test]
fn identical_sqlite_main_files_with_different_effective_state_are_archived() {
    let t = Fixture::new();
    let view = t.view("claude", "run-checkpoint");
    let database = "state_5.sqlite";
    let shared = t.home.join(database);
    {
        let db = rusqlite::Connection::open(&shared).unwrap();
        db.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE history (message TEXT); INSERT INTO history VALUES ('checkpoint');").unwrap();
    }
    fs::copy(&shared, view.join(database)).unwrap();
    let writer = rusqlite::Connection::open(&shared).unwrap();
    writer
        .execute_batch("PRAGMA wal_autocheckpoint=0; DELETE FROM history;")
        .unwrap();
    assert_eq!(
        fs::read(&shared).unwrap(),
        fs::read(view.join(database)).unwrap()
    );
    assert!(t.home.join("state_5.sqlite-wal").exists());
    assert!(!view.join("state_5.sqlite-wal").exists());

    let report = t.run(Mode::Apply);
    assert_eq!(report.totals.removed, 1, "{}", report.text());
    let archive = t
        .root
        .join("archive/claude/synthetic/run-checkpoint/state_5.sqlite.zst");
    let snapshot = zstd::stream::decode_all(File::open(archive).unwrap()).unwrap();
    let restored = t._temp.path().join("restored.sqlite");
    write(&restored, &snapshot);
    let restored = rusqlite::Connection::open(restored).unwrap();
    let value: String = restored
        .query_row("SELECT message FROM history", [], |row| row.get(0))
        .unwrap();
    assert_eq!(value, "checkpoint");
    let shared_count: u32 = writer
        .query_row("SELECT count(*) FROM history", [], |row| row.get(0))
        .unwrap();
    assert_eq!(shared_count, 0);
}

#[test]
fn linked_and_unknown_sidecars_keep_private_sqlite_groups() {
    let t = Fixture::new();
    let linked = t.view("claude", "run-linked-db");
    let unknown = t.view("claude", "run-unknown-sidecar");
    let unknown_link = t.view("claude", "run-unknown-link");
    for view in [&linked, &unknown, &unknown_link] {
        let db = rusqlite::Connection::open(view.join("private.sqlite")).unwrap();
        db.execute("CREATE TABLE history (message TEXT)", [])
            .unwrap();
    }
    let outside = t._temp.path().join("outside");
    write(&outside, b"synthetic");
    symlink(&outside, linked.join("private.sqlite-wal")).unwrap();
    symlink(
        &outside,
        unknown_link.join("private.sqlite-synthetic-sidecar"),
    )
    .unwrap();
    write(
        &unknown.join("private.sqlite-synthetic-sidecar"),
        b"synthetic",
    );

    let report = t.run(Mode::Apply);
    assert_eq!(report.totals.kept, 3);
    assert!(report.incomplete());
    assert!(
        report
            .text()
            .contains("linked or non-regular sidecar at \"private.sqlite-wal\"")
    );
    assert!(
        report.text().contains(
            "unrecognized private SQLite sidecar at \"private.sqlite-synthetic-sidecar\""
        )
    );
    assert!(linked.join("private.sqlite").is_file());
    assert!(unknown.join("private.sqlite").is_file());
    assert!(unknown_link.is_dir());
    assert_eq!(fs::read(outside).unwrap(), b"synthetic");
}

#[test]
fn codex_sqlite_groups_with_only_links_are_unlinked_without_reading_targets() {
    let t = Fixture::new();
    let view = t.view("codex", "run-linked-sqlite");
    for name in [
        "queue_1.sqlite",
        "queue_1.sqlite-wal",
        "queue_1.sqlite-shm",
        "queue_1.sqlite-other",
        ".sqlite-maintenance.lock",
    ] {
        let target = t.home.join(name);
        write(&target, b"synthetic shared data");
        symlink(&target, view.join(name)).unwrap();
    }
    let preview = t.run(Mode::Preview);
    assert_eq!(preview.totals.finished, 1, "{}", preview.text());
    assert_eq!(preview.totals.bytes_archived, 0);
    let apply = t.run(Mode::Apply);
    assert_eq!(apply.totals.removed, 1, "{}", apply.text());
    assert!(!view.exists());
    assert!(!t.root.join("archive").exists());
    assert_eq!(fs::read_dir(&t.home).unwrap().count(), 5);
    for entry in fs::read_dir(&t.home).unwrap() {
        assert_eq!(
            fs::read(entry.unwrap().path()).unwrap(),
            b"synthetic shared data"
        );
    }
}

#[test]
fn codex_full_copy_snapshots_main_and_wal_and_prunes_shared_memory() {
    let t = Fixture::new();
    let view = t.view("codex", "run-private-sqlite");
    let standalone = ["goals_1.sqlite", "memories_1.sqlite", "state_5.sqlite"];
    let create = "CREATE TABLE history (message TEXT); INSERT INTO history VALUES ('synthetic');";
    for name in standalone {
        let db = rusqlite::Connection::open(view.join(name)).unwrap();
        db.execute_batch(create).unwrap();
    }
    let queue = view.join("queue_1.sqlite");
    let db = rusqlite::Connection::open(&queue).unwrap();
    db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")
        .unwrap();
    db.execute_batch(create).unwrap();
    assert!(view.join("queue_1.sqlite-shm").is_file());
    write(&view.join("orphan.sqlite-shm"), b"synthetic unused SHM");
    let linked_shm = t.home.join("state_5.sqlite-shm");
    write(&linked_shm, b"synthetic shared SHM");
    symlink(&linked_shm, view.join("state_5.sqlite-shm")).unwrap();
    write(&view.join(".sqlite-maintenance.lock"), b"");
    for name in ["logs_2.sqlite", "logs_2.sqlite-wal", "logs_2.sqlite-shm"] {
        write(&view.join(name), b"synthetic logs");
    }

    let expected_bytes: u64 = standalone
        .into_iter()
        .chain(["queue_1.sqlite", "queue_1.sqlite-wal"])
        .map(|name| fs::metadata(view.join(name)).unwrap().len())
        .sum();
    let preview = t.run(Mode::Preview);
    assert_eq!(preview.totals.finished, 1, "{}", preview.text());
    assert_eq!(preview.totals.bytes_archived, expected_bytes);
    let apply = t.run(Mode::Apply);
    assert_eq!(apply.totals.removed, 1, "{}", apply.text());
    drop(db);
    assert!(!view.exists());
    assert_eq!(fs::read(linked_shm).unwrap(), b"synthetic shared SHM");
    let archive = t.root.join("archive/codex/synthetic/run-private-sqlite");
    assert_eq!(fs::read_dir(&archive).unwrap().count(), 5);
    for name in standalone.into_iter().chain(["queue_1.sqlite"]) {
        let payload = File::open(archive.join(format!("{name}.zst"))).unwrap();
        let restored = t._temp.path().join(name);
        write(&restored, &zstd::stream::decode_all(payload).unwrap());
        let value: String = rusqlite::Connection::open(restored)
            .unwrap()
            .query_row("SELECT message FROM history", [], |row| row.get(0))
            .unwrap();
        assert_eq!(value, "synthetic");
    }
}

#[test]
fn nonempty_sqlite_maintenance_lock_keeps_view_and_reports_relative_path() {
    let t = Fixture::new();
    let view = t.view("codex", "run-lock");
    let relative = "nested/.sqlite-maintenance.lock";
    write(&view.join(relative), b"synthetic nonempty lock");
    let apply = t.run(Mode::Apply);
    assert!(apply.incomplete());
    assert_eq!(apply.totals.kept, 1, "{}", apply.text());
    assert_eq!(
        apply.groups[0].views[0].reason,
        format!("unrecognized private SQLite sidecar at {relative:?}")
    );
    assert_eq!(
        fs::read(view.join(relative)).unwrap(),
        b"synthetic nonempty lock"
    );
    write(&view.join(relative), b"");
    let retry = t.run(Mode::Apply);
    assert_eq!(retry.totals.removed, 1, "{}", retry.text());
    assert!(!t.root.join("archive").exists());
}

#[test]
fn orphan_wal_keeps_later_commits_absent_from_an_older_archive() {
    let t = Fixture::new();
    let view = t.view("claude", "run-orphan");
    let main = view.join("private.sqlite");
    {
        let db = rusqlite::Connection::open(&main).unwrap();
        db.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE history (message TEXT); INSERT INTO history VALUES ('earlier checkpoint');").unwrap();
    }
    let archive = t.root.join("archive/claude/synthetic/run-orphan");
    let first = t.run_with(
        Mode::Apply,
        Duration::from_secs(3 * 86400),
        &mut |_| Ok(archive.exists()),
        &mut || Ok(0.0),
    );
    assert!(first.incomplete());
    let earlier = fs::read(archive.join("private.sqlite.zst")).unwrap();

    let later = rusqlite::Connection::open(&main).unwrap();
    later
        .execute_batch("PRAGMA wal_autocheckpoint=0; INSERT INTO history VALUES ('later commit');")
        .unwrap();
    let raw_wal = fs::read(view.join("private.sqlite-wal")).unwrap();
    let recovery = t._temp.path().join("recovery/private.sqlite");
    write(&recovery, &fs::read(&main).unwrap());
    fs::remove_file(main).unwrap();

    let no_archive = t.view("claude", "run-no-snapshot");
    write(
        &no_archive.join("private.sqlite-journal"),
        b"synthetic orphan journal",
    );
    let preview = t.run(Mode::Preview);
    assert_eq!(preview.totals.finished, 2, "{}", preview.text());
    assert!(preview.totals.bytes_archived >= raw_wal.len() as u64);
    assert!(!archive.join("orphan-sidecars").exists());
    let retry = t.run(Mode::Apply);
    assert_eq!(retry.totals.removed, 2, "{}", retry.text());
    assert!(!view.exists());
    assert!(!no_archive.exists());
    drop(later);
    assert_eq!(
        fs::read(archive.join("private.sqlite.zst")).unwrap(),
        earlier
    );
    assert!(
        fs::read_dir(archive.parent().unwrap())
            .unwrap()
            .all(|entry| {
                !entry
                    .unwrap()
                    .file_name()
                    .as_encoded_bytes()
                    .starts_with(b".aswap-archive-")
            })
    );
    let supplement = archive.join("orphan-sidecars");
    let versions: Vec<_> = fs::read_dir(&supplement)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    let compressed = versions
        .iter()
        .map(|dir| dir.join("private.sqlite-wal.zst"))
        .find(|path| path.is_file())
        .unwrap();
    let decoded = zstd::stream::decode_all(File::open(compressed).unwrap()).unwrap();
    assert_eq!(decoded, raw_wal);
    write(&recovery.with_file_name("private.sqlite-wal"), &decoded);
    let recovered = rusqlite::Connection::open(&recovery).unwrap();
    let recovered_rows: u32 = recovered
        .query_row("SELECT count(*) FROM history", [], |row| row.get(0))
        .unwrap();
    let earlier_path = t._temp.path().join("earlier.sqlite");
    write(
        &earlier_path,
        &zstd::stream::decode_all(&earlier[..]).unwrap(),
    );
    let earlier_db = rusqlite::Connection::open(earlier_path).unwrap();
    let earlier_rows: u32 = earlier_db
        .query_row("SELECT count(*) FROM history", [], |row| row.get(0))
        .unwrap();
    assert_eq!((earlier_rows, recovered_rows), (1, 2));

    let manifest = fs::read(archive.join("SHA256SUMS")).unwrap();
    assert!(String::from_utf8_lossy(&manifest).contains("private.sqlite-wal.zst"));
    write(&view.join("private.sqlite-wal"), &raw_wal);
    let identical = t.run(Mode::Apply);
    assert_eq!(identical.totals.removed, 1, "{}", identical.text());
    assert_eq!(manifest, fs::read(archive.join("SHA256SUMS")).unwrap());
    assert_eq!(fs::read_dir(supplement).unwrap().count(), versions.len());
}

#[test]
fn changing_the_archive_during_exchange_keeps_both_generations_and_the_view() {
    for replace_directory in [true, false] {
        let t = Fixture::new();
        let view = t.view("claude", "run-supplement");
        let restored_view = t.view("claude", "run-restored");
        let archives = t.root.join("archive/claude/synthetic");
        let archive = archives.join("run-supplement");
        let restored = archives.join("run-restored");
        let original = b"synthetic original archive";
        let restored_data = b"synthetic restored archive";
        write(&view.join("original.jsonl"), original);
        write(&restored_view.join("restored.jsonl"), restored_data);
        let setup = t.run_with(
            Mode::Apply,
            Duration::from_secs(3 * 86400),
            &mut |candidate| Ok(candidate == view && archive.exists()),
            &mut || Ok(0.0),
        );
        assert_eq!(setup.totals.removed, 1, "{}", setup.text());
        let orphan = view.join("private.sqlite-wal");
        write(&orphan, b"synthetic orphan WAL");
        let saved = archives.join("saved");
        let canonical = archive.clone();
        let original_name = saved.clone();
        archive::BEFORE_EXCHANGE.set(Some(Box::new(move || {
            if replace_directory {
                fs::rename(&canonical, original_name).unwrap();
                fs::rename(restored, &canonical).unwrap();
            } else {
                fs::remove_file(canonical.join("original.jsonl.zst")).unwrap();
                fs::copy(
                    restored.join("restored.jsonl.zst"),
                    canonical.join("restored.jsonl.zst"),
                )
                .unwrap();
                fs::copy(restored.join("SHA256SUMS"), canonical.join("SHA256SUMS")).unwrap();
            }
        })));

        let report = t.run(Mode::Apply);
        assert!(report.incomplete());
        assert_eq!(report.totals.removed, 0, "{}", report.text());
        assert!(
            report
                .text()
                .contains("displaced archive changed during exchange")
        );
        assert_eq!(fs::read(&orphan).unwrap(), b"synthetic orphan WAL");
        assert_eq!(fs::read(view.join("original.jsonl")).unwrap(), original);
        let decode = |path| zstd::stream::decode_all(File::open(path).unwrap()).unwrap();
        assert_eq!(decode(archive.join("original.jsonl.zst")), original);
        assert!(archive.join("orphan-sidecars").is_dir());
        let displaced: Vec<_> = fs::read_dir(&archives)
            .unwrap()
            .map(|entry| entry.unwrap())
            .filter(|entry| {
                entry
                    .file_name()
                    .as_encoded_bytes()
                    .starts_with(b".aswap-archive-")
            })
            .collect();
        assert_eq!(displaced.len(), 1);
        assert_eq!(
            decode(displaced[0].path().join("restored.jsonl.zst")),
            restored_data
        );
        assert!(displaced[0].path().join("SHA256SUMS").is_file());
        if replace_directory {
            assert_eq!(decode(saved.join("original.jsonl.zst")), original);
        }
    }
}

#[test]
fn ambiguous_descriptor_snapshot_rechecks_zombies_and_cmdline() {
    let t = Fixture::new();
    let view = t.view("codex", "run-fd-recheck");
    let proc = t._temp.path().join("proc");
    let exiting = proc.join("321");
    let helper = proc.join("322");
    let provider = proc.join("323");
    for pid in [&exiting, &helper, &provider] {
        write(
            &pid.join("status"),
            b"State:\tS\nUid:\t1234\t1234\t1234\t1234\n",
        );
        write(&pid.join("fd/4"), b"synthetic");
    }
    write(&exiting.join("cmdline"), b"");
    write(
        &provider.join("cmdline"),
        b"/bin/node\0synthetic-secret-argument\0",
    );
    let snapshot = process::Fds::read_with_links(&proc, 1234, &[&view], None, |_| {
        Err(ErrorKind::PermissionDenied.into())
    })
    .unwrap();
    write(
        &exiting.join("status"),
        b"State:\tZ\nUid:\t1234\t1234\t1234\t1234\n",
    );
    write(&helper.join("cmdline"), b"sshd-session: synthetic\0");

    let diagnostic = Cell::new(None);
    let settings = t.settings();
    let report = Retention {
        root: &t.root,
        at: SystemTime::now() + Duration::from_secs(3 * 86400),
        in_use: &mut |path| {
            record_liveness(
                snapshot.uses_view(path, &mut Default::default()),
                &diagnostic,
            )
        },
        liveness_error: &diagnostic,
        recent_fds: &mut |_, _| Ok(false),
        mounts: &mut || Ok(mounts::Mounts::default()),
        pressure: Pressure {
            read: &mut || Ok(0.0),
            limit: 30.0,
            next: Instant::now(),
            stopped: None,
        },
    }
    .run(settings, Mode::Preview)
    .unwrap();
    let reason = &report.groups[0].views[0].reason;
    assert!(reason.contains("PID 323, cmdline excerpt node"), "{reason}");
    assert!(!reason.contains("synthetic-secret-argument"));

    write(
        &provider.join("status"),
        b"State:\tX\nUid:\t1234\t1234\t1234\t1234\n",
    );
    let mut ignored = Default::default();
    assert!(!snapshot.uses_view(&view, &mut ignored).unwrap());
    assert_eq!(
        ignored.into_iter().collect::<Vec<_>>(),
        [OsString::from("322")]
    );
}

#[test]
fn descriptor_opened_after_initial_snapshot_restores_retiring_view() {
    for use_old_name in [true, false] {
        let t = Fixture::new();
        let view = t.view("codex", "run-late-fd");
        let relative = "sessions/rollout-synthetic.jsonl";
        write(&view.join(relative), b"synthetic transcript");
        let proc = t._temp.path().join("proc");
        let pid = proc.join("321");
        write(
            &pid.join("status"),
            b"State:\tS\nUid:\t1234\t1234\t1234\t1234\n",
        );
        fs::create_dir_all(pid.join("fd")).unwrap();
        let initial = process::Fds::read(&proc, 1234, &[&t.root.join("runtimes")], None).unwrap();
        let mut opened = false;
        let report = Retention {
            root: &t.root,
            at: SystemTime::now() + Duration::from_secs(3 * 86400),
            in_use: &mut |path| {
                initial
                    .uses_view(path, &mut Default::default())
                    .map_err(Error::from)
            },
            liveness_error: &Cell::new(None),
            recent_fds: &mut |old, tombstone| {
                assert!(!old.exists());
                assert!(tombstone.join(relative).is_file());
                let opened_path = if use_old_name { old } else { tombstone };
                symlink(opened_path.join(relative), pid.join("fd/4")).unwrap();
                opened = true;
                let fresh = process::Fds::read(&proc, 1234, &[old, tombstone], None).unwrap();
                let mut ignored = Default::default();
                Ok(fresh.uses_view(old, &mut ignored)?
                    || fresh.uses_view(tombstone, &mut ignored)?)
            },
            mounts: &mut || Ok(mounts::Mounts::default()),
            pressure: Pressure {
                read: &mut || Ok(0.0),
                limit: 30.0,
                next: Instant::now(),
                stopped: None,
            },
        }
        .run(t.settings(), Mode::Apply)
        .unwrap();
        assert!(opened);
        assert!(report.incomplete());
        assert!(
            report
                .text()
                .contains("open file descriptor refers to the retiring view")
        );
        assert!(view.join(relative).exists());
        assert!(!view.with_file_name(".retiring-run-late-fd").exists());
    }
}

#[test]
fn removing_or_replacing_only_a_sealed_payload_keeps_original_data() {
    for remove in [true, false] {
        let t = Fixture::new();
        let view = t.view("claude", "run-payload");
        let archive = t.root.join("archive/claude/synthetic/run-payload");
        let payload = archive.join("history.jsonl.zst");
        let manifest = archive.join("SHA256SUMS");
        let original = b"synthetic history";
        write(&view.join("history.jsonl"), original);
        let report = t.run_with(
            Mode::Apply,
            Duration::from_secs(3 * 86400),
            &mut |path| {
                if path.ends_with(".retiring-run-payload") {
                    assert!(manifest.is_file());
                    if remove {
                        fs::remove_file(&payload).unwrap();
                    } else {
                        let mut substitute = tempfile::NamedTempFile::new_in(&archive).unwrap();
                        std::io::Write::write_all(&mut substitute, &fs::read(&payload).unwrap())
                            .unwrap();
                        substitute.persist(&payload).unwrap();
                    }
                    assert!(manifest.is_file());
                }
                Ok(false)
            },
            &mut || Ok(0.0),
        );
        assert!(report.incomplete());
        assert_eq!(report.totals.removed, 0);
        let reason = if remove {
            "archive checksum manifest did not verify"
        } else {
            "published archive payload identity changed"
        };
        assert!(report.text().contains(reason), "{}", report.text());
        assert_eq!(fs::read(view.join("history.jsonl")).unwrap(), original);
    }
}

#[test]
fn tombstone_identity_mismatch_leaves_inventoried_and_replacement_directories_intact() {
    let t = Fixture::new();
    let view = t.view("claude", "run-swap");
    write(&view.join("original"), b"synthetic");
    let inventoried = Dir::open(&view).unwrap();
    let parent = Dir::open(view.parent().unwrap()).unwrap();
    let saved = view.with_file_name("original-view");
    fs::rename(&view, &saved).unwrap();
    write(&view.join("replacement"), b"synthetic replacement");
    let mut pressure = Pressure {
        read: &mut || Ok(0.0),
        limit: 30.0,
        next: Instant::now(),
        stopped: None,
    };
    let mut reclaimed = 0;
    let error = retire::remove(
        &parent,
        &view,
        &inventoried,
        |_, _| panic!("replacement must never be verified or deleted"),
        &mut pressure,
        &mut reclaimed,
    )
    .unwrap_err();
    assert_eq!(error.0, "retiring directory differs from inventoried view");
    assert_eq!(reclaimed, 0);
    assert!(saved.join("original").exists());
    assert!(
        view.with_file_name(".retiring-run-swap")
            .join("replacement")
            .exists()
    );
}

#[test]
fn operational_failure_before_publication_makes_apply_incomplete() {
    let t = Fixture::new();
    let view = t.view("claude", "run-healthy");
    write(&view.with_file_name("run-unreadable"), b"not a directory");
    let preview = t.run(Mode::Preview);
    assert!(!preview.incomplete());
    let applied = t.run(Mode::Apply);
    assert!(applied.incomplete());
    assert!(
        applied
            .text()
            .contains("unsafe or unreadable view directory")
    );
}

#[test]
fn the_gc_lock_waits_out_a_brief_holder_but_not_another_gc() {
    let t = tempfile::tempdir().unwrap();
    let ours = fs::File::open(t.path()).unwrap();
    let other = fs::File::open(t.path()).unwrap();
    // A brief holder (a forked child before exec) lets go within patience.
    fs2::FileExt::lock_exclusive(&other).unwrap();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(200));
            fs2::FileExt::unlock(&other).unwrap();
        });
        let held = lock_runtimes(&ours, Duration::from_secs(5)).unwrap();
        // A forked child would share the description; dropping the guard
        // releases the lock anyway.
        let shared = ours.try_clone().unwrap();
        drop(held);
        fs2::FileExt::try_lock_exclusive(&other).unwrap();
        fs2::FileExt::unlock(&other).unwrap();
        drop(shared);
    });
    // Another GC holds it throughout.
    fs2::FileExt::lock_exclusive(&other).unwrap();
    let failure = lock_runtimes(&ours, Duration::from_millis(200)).unwrap_err();
    assert_eq!(
        failure.0,
        "cannot lock runtimes directory (another GC may be active)"
    );
}
