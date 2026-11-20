//! Codex records each thread's rollout path in its shared state database
//! as spelled under `CODEX_HOME`. A run in a launch view must leave the
//! home's path there, not the view's (removed after the run): on
//! 2026-11-19, `codex resume` lost five transcripts that way.
use aswap::{
    provider::{self, Provider},
    shared::{View, ViewCleanup},
    store::{self, Store},
};
use rusqlite::{Connection, params};
use serde_json::json;
use std::{path::Path, process::Command};

const ID: &str = "01a1f1fe-7601-7603-9212-031af3c72201";
const GONE: &str = "01a1f204-ef0a-7792-9e77-0cb931110bd3";
const DAY: &str = "sessions/2026/11/19";

fn rollout(id: &str) -> String {
    format!("rollout-2026-11-19T04-09-07-{id}.jsonl")
}
fn path_of(db: &Path, id: &str) -> String {
    Connection::open(db)
        .unwrap()
        .query_row(
            "SELECT rollout_path FROM threads WHERE id = ?1",
            [id],
            |r| r.get(0),
        )
        .unwrap()
}

#[test]
fn a_finished_run_leaves_the_homes_rollout_path_in_codexs_index() {
    let t = tempfile::tempdir().unwrap();
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "rollout_child", "--nocapture"])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("HOME", t.path())
        .env("XDG_DATA_HOME", t.path().join("data"))
        .env("CODEX_HOME", t.path().join("codex"))
        .env("ASWAP_ROLLOUT_CHILD", "1")
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
fn rollout_child() {
    if std::env::var("ASWAP_ROLLOUT_CHILD").as_deref() != Ok("1") {
        return;
    }
    let store = Store::from_env().unwrap();
    let home = Provider::Codex.current_home().unwrap();
    std::fs::create_dir_all(home.join(DAY)).unwrap();
    store::json_write(
        &home.join("auth.json"),
        &json!({"tokens":{"access_token":"synthetic-access"}}),
    )
    .unwrap();
    let db = home.join("state_5.sqlite");
    Connection::open(&db)
        .unwrap()
        .execute_batch("CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL);")
        .unwrap();
    let a = provider::add(&store, Provider::Codex, "one").unwrap();
    let views = store.root.join("runtimes/codex");

    // An older run whose view is gone left a dangling row; its transcript
    // is intact in the home.
    std::fs::write(home.join(DAY).join(rollout(GONE)), "{}\n").unwrap();
    let gone = views.join("one/run-Gone00").join(DAY).join(rollout(GONE));
    Connection::open(&db)
        .unwrap()
        .execute(
            "INSERT INTO threads VALUES (?1, ?2)",
            params![GONE, gone.to_str().unwrap()],
        )
        .unwrap();
    // The report-only check finds it, says where the transcript is, and
    // changes nothing.
    let check = Command::new(env!("CARGO_BIN_EXE_aswap"))
        .args(["--json", "check", "codex"])
        .output()
        .unwrap();
    assert_eq!(check.status.code(), Some(1), "{check:?}");
    let report: serde_json::Value = serde_json::from_slice(&check.stdout).unwrap();
    assert_eq!(report["dangling"], 1);
    assert_eq!(
        report["entries"][0]["entry"]["canonical"],
        home.join(DAY).join(rollout(GONE)).to_str().unwrap()
    );
    assert_eq!(path_of(&db, GONE), gone.to_str().unwrap());

    // A run: Codex in the view writes its rollout and indexes it as
    // `$CODEX_HOME/sessions/...`, through the view's links.
    let view = View::prepare(&store, &a).unwrap();
    let mut cleanup = ViewCleanup::new(&store, &view);
    // The launch swept the dangling row from the older run.
    assert_eq!(
        path_of(&db, GONE),
        home.join(DAY).join(rollout(GONE)).to_str().unwrap()
    );
    let through_view = view.path.join(DAY).join(rollout(ID));
    std::fs::write(&through_view, "{}\n").unwrap();
    Connection::open(view.path.join("state_5.sqlite"))
        .unwrap()
        .execute(
            "INSERT INTO threads VALUES (?1, ?2)",
            params![ID, through_view.to_str().unwrap()],
        )
        .unwrap();
    cleanup.synced = true;
    drop(cleanup);
    assert!(!view.path.exists(), "the view is removed after the run");
    let canonical = home.join(DAY).join(rollout(ID));
    assert_eq!(path_of(&db, ID), canonical.to_str().unwrap());
    assert!(canonical.is_file());

    let check = Command::new(env!("CARGO_BIN_EXE_aswap"))
        .args(["--json", "check", "codex"])
        .output()
        .unwrap();
    assert_eq!(check.status.code(), Some(0), "{check:?}");
    let report: serde_json::Value = serde_json::from_slice(&check.stdout).unwrap();
    assert_eq!(report["dangling"], 0);
    assert_eq!(report["in_view"], 0);
}
