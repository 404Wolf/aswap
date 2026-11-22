//! The newer-version notice end to end: `aswap update-check` against a local
//! stand-in for GitHub's tags API, then where the notice may and may not
//! appear. No real network.
use std::{
    io::{BufRead, BufReader, Write},
    net::TcpListener,
    path::Path,
    process::{Command, Output},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

/// Serves `status` and `body` to every request; counts the requests.
fn server(status: u16, body: &'static str) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/tags", listener.local_addr().unwrap());
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap() > 0 && line != "\r\n" {
                line.clear();
            }
            counter.fetch_add(1, Ordering::SeqCst);
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    (url, hits)
}

fn aswap(root: &Path, url: &str, args: &[&str], extra: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_aswap"));
    command
        .args(args)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("HOME", root)
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("CLAUDE_CONFIG_DIR", root.join("claude"))
        .env("CODEX_HOME", root.join("codex"))
        .env("ASWAP_UPDATE_URL", url);
    for (key, value) in extra {
        command.env(key, value);
    }
    command.output().unwrap()
}

fn cache(root: &Path) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(root.join("cache/aswap/update-check.json")).unwrap())
        .unwrap()
}

const TAGS: &str = r#"[{"name":"v99.0.0-preview.x.1"},{"name":"v99.0.0"},{"name":"v0.2.1"},{"name":"preview/gemini-0f0290a"}]"#;

#[test]
fn a_check_caches_the_newest_release_once_a_day() {
    let t = tempfile::tempdir().unwrap();
    let (url, hits) = server(200, TAGS);
    let out = aswap(t.path(), &url, &["update-check"], &[]);
    assert!(out.status.success());
    assert!(out.stdout.is_empty() && out.stderr.is_empty());
    assert_eq!(cache(t.path())["latest"], "99.0.0");
    assert_eq!(cache(t.path())["failures"], 0);
    // Not due again for a day.
    aswap(t.path(), &url, &["update-check"], &[]);
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[test]
fn a_failed_check_is_silent_and_backs_off() {
    let t = tempfile::tempdir().unwrap();
    let (url, hits) = server(403, r#"{"message":"rate limited"}"#);
    let out = aswap(t.path(), &url, &["update-check"], &[]);
    assert!(out.status.success());
    assert!(out.stdout.is_empty() && out.stderr.is_empty());
    let c = cache(t.path());
    assert_eq!(c["failures"], 1);
    assert!(c["latest"].is_null());
    assert!(c["next_check_at"].as_u64().unwrap() > aswap::now() + 86_000);
    aswap(t.path(), &url, &["update-check"], &[]);
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    // Nothing listening: still silent.
    let t = tempfile::tempdir().unwrap();
    let out = aswap(t.path(), "http://127.0.0.1:9/tags", &["update-check"], &[]);
    assert!(out.status.success() && out.stderr.is_empty());
    assert_eq!(cache(t.path())["failures"], 1);
}

#[test]
fn the_notice_goes_to_stderr_only_where_allowed() {
    let t = tempfile::tempdir().unwrap();
    let (url, hits) = server(200, TAGS);
    aswap(t.path(), &url, &["update-check"], &[]);
    let notice = "99.0.0 is available";
    // `status` shows it on stderr, even off a terminal; stdout stays clean.
    let status = aswap(t.path(), &url, &["status"], &[]);
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    assert!(String::from_utf8_lossy(&status.stderr).contains(notice));
    assert!(!String::from_utf8_lossy(&status.stdout).contains(notice));
    // Never with --json, never off a terminal for other commands, never
    // from commands that wrap or serve something.
    for args in [
        &["status", "--json"][..],
        &["list", "--json"],
        &["list"],
        &["usage", "--json"],
        &["sessions"],
        &["waybar", "--once"],
    ] {
        let out = aswap(t.path(), &url, args, &[]);
        assert!(
            !String::from_utf8_lossy(&out.stderr).contains(notice)
                && !String::from_utf8_lossy(&out.stdout).contains(notice),
            "{args:?}"
        );
    }
    // Opted out.
    let off = aswap(
        t.path(),
        &url,
        &["status"],
        &[("ASWAP_NO_UPDATE_CHECK", "1")],
    );
    assert!(!String::from_utf8_lossy(&off.stderr).contains(notice));
    std::fs::create_dir_all(t.path().join("config/aswap")).unwrap();
    std::fs::write(
        t.path().join("config/aswap/config.toml"),
        "[updates]\ncheck = false\n",
    )
    .unwrap();
    let off = aswap(t.path(), &url, &["status"], &[]);
    assert!(off.status.success());
    assert!(!String::from_utf8_lossy(&off.stderr).contains(notice));
    let checked = aswap(t.path(), &url, &["update-check"], &[]);
    assert!(checked.status.success());
    // Off a terminal, no command ever started a check of its own.
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}
