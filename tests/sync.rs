use serde_json::json;
use std::{fs, os::unix::fs::PermissionsExt, process::Command};
#[test]
fn ssh_sync_uses_private_transport_and_reports_only_counts() {
    let t = tempfile::tempdir().unwrap();
    let bin = t.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let packet = json!({"protocol":1,"issued_at":1,"profiles":[{"account":{"provider":"claude","label":"one"},"auth":{"claudeAiOauth":{"accessToken":"synthetic-private-owner-access","refreshToken":"synthetic-private-forbidden-refresh"}},"config":{"oauthAccount":{"accountUuid":"synthetic-one","emailAddress":"alice@example.com"}},"cache":{"usage":{"windows":[]},"fetched_at":0,"next_poll_at":9999999999u64,"failures":0,"error":null}}]});
    let fixture = t.path().join("owner-packet");
    fs::write(&fixture, serde_json::to_vec(&packet).unwrap()).unwrap();
    let ssh = bin.join("ssh");
    fs::write(&ssh,"#!/bin/sh\ncase \"$*\" in *'aswap sync --serve'*) ;; *) exit 7;; esac\nread request\ncat \"$ASWAP_TEST_PACKET\"\n").unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
    let path = std::env::join_paths(
        std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let invoke = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_aswap"))
            .args(args)
            .env_clear()
            .env("PATH", &path)
            .env("HOME", t.path())
            .env("XDG_DATA_HOME", t.path().join("data"))
            .env("ASWAP_TEST_PACKET", &fixture)
            .output()
            .unwrap()
    };
    let out = invoke(&["sync", "synthetic-owner", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["profiles"], 1);
    let output = String::from_utf8(out.stdout).unwrap();
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&output)
        && v.get("accounts").is_some()
    {
        assert!(v["accounts"][0]["usage_error"].is_null());
    }
    assert!(!output.contains("synthetic-private"));
    assert!(!output.contains("alice@example.com"));
    let local: serde_json::Value = serde_json::from_slice(
        &fs::read(
            t.path()
                .join("data/aswap/profiles/claude/one/.credentials.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(local["claudeAiOauth"].get("refreshToken").is_none());
    // An explicit quota refresh traverses the owner pipe; stale local backoff is irrelevant.
    let out = invoke(&["usage", "claude", "--refresh", "--json"]);
    assert!(out.status.success());
    let output = String::from_utf8(out.stdout).unwrap();
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&output)
        && v.get("accounts").is_some()
    {
        assert!(v["accounts"][0]["usage_error"].is_null());
    }
    assert!(!output.contains("synthetic-private"));
    assert!(!output.contains("alice@example.com"));
    fs::write(
        &ssh,
        "#!/bin/sh\necho synthetic-private-error >&2\nexit 1\n",
    )
    .unwrap();
    let out = invoke(&["sync", "--json"]);
    assert!(!out.status.success());
    assert!(!String::from_utf8_lossy(&out.stderr).contains("synthetic-private-error"));
}
