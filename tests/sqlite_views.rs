//! A launch view shares the home's SQLite databases. Every spelling of a
//! database's -wal and -shm must reach the home's, so no client can make a
//! WAL of its own for a shared database. On 2026-11-20, 6 of 313 Codex
//! views lacked these links because the files didn't exist when the view
//! was made.
use aswap::{
    provider::{self, Provider},
    shared::View,
    store::{self, Store},
};
use rusqlite::Connection;
use serde_json::json;
use std::{fs, os::unix::fs::symlink, path::Path, process::Command};

fn regular_sidecars(view: &Path) -> Vec<String> {
    fs::read_dir(view)
        .unwrap()
        .flatten()
        .filter(|e| e.file_type().unwrap().is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with("-wal") || n.ends_with("-shm"))
        .collect()
}
fn check(json: bool) -> (Option<i32>, serde_json::Value) {
    let mut args = vec!["check", "codex"];
    if json {
        args.insert(0, "--json");
    }
    let out = Command::new(env!("CARGO_BIN_EXE_aswap"))
        .args(args)
        .output()
        .unwrap();
    (
        out.status.code(),
        serde_json::from_slice(&out.stdout).unwrap_or_default(),
    )
}

#[test]
fn a_view_shares_one_wal_with_the_home_even_before_it_exists() {
    let t = tempfile::tempdir().unwrap();
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "sqlite_child", "--nocapture"])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("HOME", t.path())
        .env("XDG_DATA_HOME", t.path().join("data"))
        .env("CODEX_HOME", t.path().join("codex"))
        .env("ASWAP_SQLITE_CHILD", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
#[test]
fn sqlite_child() {
    if std::env::var("ASWAP_SQLITE_CHILD").as_deref() != Ok("1") {
        return;
    }
    let store = Store::from_env().unwrap();
    let home = Provider::Codex.current_home().unwrap();
    fs::create_dir_all(&home).unwrap();
    store::json_write(
        &home.join("auth.json"),
        &json!({"tokens":{"access_token":"synthetic-access"}}),
    )
    .unwrap();
    let db = home.join("state_5.sqlite");
    // A WAL database whose last connection closed: no -wal or -shm now.
    {
        let c = Connection::open(&db).unwrap();
        c.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE t (x INTEGER);")
            .unwrap();
    }
    assert!(!home.join("state_5.sqlite-wal").exists());
    let a = provider::add(&store, Provider::Codex, "one").unwrap();
    let view = View::prepare(&store, &a).unwrap();
    for suffix in ["-wal", "-shm"] {
        let link = view.path.join(format!("state_5.sqlite{suffix}"));
        assert_eq!(
            fs::read_link(&link).unwrap(),
            home.join(format!("state_5.sqlite{suffix}")),
            "{suffix} is linked before it exists"
        );
    }
    // A writer through the view and a reader through the home share one WAL.
    let writer = Connection::open(view.path.join("state_5.sqlite")).unwrap();
    writer.execute("INSERT INTO t VALUES (1)", []).unwrap();
    let reader = Connection::open(&db).unwrap();
    let n: i64 = reader
        .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1);
    assert!(home.join("state_5.sqlite-wal").is_file());
    assert!(regular_sidecars(&view.path).is_empty());
    let (code, report) = check(true);
    assert_eq!(code, Some(0), "{report}");
    assert_eq!(report["sqlite_at_risk"], 0);
    drop((writer, reader));

    // Views from before the fix, made by hand here: one without sidecar
    // links (safe), one with a private WAL beside its linked database.
    let runs = store.root.join("runtimes/codex/one");
    let (old, broken) = (runs.join("run-Old000"), runs.join("run-Wal000"));
    for v in [&old, &broken] {
        fs::create_dir(v).unwrap();
        symlink(&db, v.join("state_5.sqlite")).unwrap();
    }
    fs::write(broken.join("state_5.sqlite-wal"), b"").unwrap();
    symlink(
        home.join("state_5.sqlite-shm"),
        broken.join("state_5.sqlite-shm"),
    )
    .unwrap();
    let (code, report) = check(true);
    assert_eq!(code, Some(1), "{report}");
    assert_eq!(report["sqlite_at_risk"], 1);
    let risks = report["sqlite_views"].as_array().unwrap();
    let of = |view: &Path| -> Vec<&str> {
        risks
            .iter()
            .filter(|r| r["view"]["view"] == view.to_str().unwrap())
            .map(|r| r["view"]["risk"].as_str().unwrap())
            .collect()
    };
    assert_eq!(of(&old), ["unlinked-sidecar", "unlinked-sidecar"]);
    assert_eq!(of(&broken), ["private-sidecar"]);
    assert!(of(&view.path).is_empty());
    // A database recreated as a private file while its sidecars still
    // link to the home's: the worst case, flagged on its own.
    let recreated = runs.join("run-New000");
    fs::create_dir(&recreated).unwrap();
    fs::write(recreated.join("state_5.sqlite"), b"").unwrap();
    symlink(
        home.join("state_5.sqlite-wal"),
        recreated.join("state_5.sqlite-wal"),
    )
    .unwrap();
    let (code, report) = check(true);
    assert_eq!(code, Some(1), "{report}");
    let risks = report["sqlite_views"].as_array().unwrap();
    let of = |view: &Path| -> Vec<&str> {
        risks
            .iter()
            .filter(|r| r["view"]["view"] == view.to_str().unwrap())
            .map(|r| r["view"]["risk"].as_str().unwrap())
            .collect()
    };
    assert_eq!(of(&recreated), ["private-copy-shared-wal", "private-copy"]);
    // The check changed nothing.
    assert!(broken.join("state_5.sqlite-wal").is_file());
    assert!(!old.join("state_5.sqlite-wal").exists());
}
