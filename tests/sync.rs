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

#[test]
fn real_transport_round_trip_diagnoses_owner_role_and_preserves_duplicate_aliases() {
    use aswap::{
        coordination,
        provider::Provider,
        store::{self, Account, Store},
        usage::Cached,
    };
    let t = tempfile::tempdir().unwrap();
    let owner_data = t.path().join("owner-data");
    let owner = Store::open(owner_data.join("aswap")).unwrap();
    let mut state = owner.load().unwrap();
    for label in ["original", "alias"] {
        let account = Account {
            provider: Provider::Claude,
            label: label.into(),
        };
        let profile = owner.profile(account.provider, label).unwrap();
        store::json_write(&profile.join(".credentials.json"),&json!({"claudeAiOauth":{"accessToken":"synthetic-transport-access","refreshToken":"synthetic-transport-refresh"}})).unwrap();
        store::json_write(&profile.join(".claude.json"),&json!({"oauthAccount":{"accountUuid":"synthetic-transport-identity","emailAddress":"alice@example.com"}})).unwrap();
        store::json_write(
            &profile.join("usage-cache.json"),
            &Cached {
                fetched_at: aswap::now(),
                next_poll_at: aswap::now() + 3600,
                ..Default::default()
            },
        )
        .unwrap();
        state.accounts.push(account);
    }
    owner.save(&state).unwrap();
    let bin = t.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let ssh = bin.join("ssh");
    fs::write(&ssh,"#!/bin/sh\nexport XDG_DATA_HOME=\"$ASWAP_TEST_OWNER_DATA\"\nexport SSH_CONNECTION=synthetic\nexec \"$ASWAP_TEST_BINARY\" sync --serve\n").unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
    let path = std::env::join_paths(
        std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let invoke = || {
        Command::new(env!("CARGO_BIN_EXE_aswap"))
            .args(["sync", "synthetic-owner", "--json"])
            .env_clear()
            .env("PATH", &path)
            .env("HOME", t.path())
            .env("XDG_DATA_HOME", t.path().join("peer-data"))
            .env("ASWAP_TEST_OWNER_DATA", &owner_data)
            .env("ASWAP_TEST_BINARY", env!("CARGO_BIN_EXE_aswap"))
            .output()
            .unwrap()
    };
    // The service may have announced owner earlier, but its present role is authoritative.
    coordination::owner(&owner).unwrap();
    coordination::off(&owner).unwrap();
    let failed = invoke();
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("owner-not-configured"));
    assert!(String::from_utf8_lossy(&failed.stderr).contains("remote-exit-1"));
    assert!(failed.stdout.is_empty());
    coordination::owner(&owner).unwrap();
    let success = invoke();
    assert!(
        success.status.success(),
        "{}",
        String::from_utf8_lossy(&success.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&success.stdout).unwrap();
    assert_eq!(value["profiles"], 2);
    assert!(String::from_utf8_lossy(&success.stderr).contains("duplicate-login warning: 1"));
    let peer = Store::open(t.path().join("peer-data/aswap")).unwrap();
    assert_eq!(peer.load().unwrap().accounts.len(), 2);
    for label in ["original", "alias"] {
        let auth: serde_json::Value = store::json_read(
            &peer
                .profile(Provider::Claude, label)
                .unwrap()
                .join(".credentials.json"),
        )
        .unwrap();
        assert!(auth["claudeAiOauth"].get("refreshToken").is_none());
    }
    // Even a later owner profile failure must not expose arbitrary remote text.
    for (script, expected) in [
        (
            "printf 'aswap: sync server requires the configured refresh owner\\n' >&2; exit 1",
            "owner-not-configured",
        ),
        (
            "printf 'aswap: sync: version-mismatch\\n' >&2; exit 1",
            "version-mismatch",
        ),
        (
            "printf 'synthetic-secret alice@example.com\\n' >&2; exit 7",
            "remote-exit-7",
        ),
        (
            "printf 'aswap: sync: version-mismatch synthetic-secret\\n' >&2; exit 255",
            "remote-exit-255",
        ),
        (
            "printf 'aswap: sync: version-mismatch\\nsynthetic-secret alice@example.com\\n' >&2; exit 1",
            "version-mismatch",
        ),
        (
            "printf '{\"protocol\":2,\"issued_at\":0,\"profiles\":[]}'",
            "version-mismatch",
        ),
        (
            "printf 'synthetic-secret alice@example.com'",
            "bad-response",
        ),
    ] {
        fs::write(&ssh, format!("#!/bin/sh\n{script}\n")).unwrap();
        let result = invoke();
        assert!(!result.status.success());
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert!(stderr.contains(expected), "expected {expected}: {stderr}");
        assert!(result.stdout.is_empty());
        assert!(!stderr.contains("synthetic-secret") && !stderr.contains("alice@example.com"));
    }
    for bytes in [&failed.stderr, &success.stderr, &success.stdout] {
        let output = String::from_utf8_lossy(bytes);
        assert!(!output.contains("alice@example.com") && !output.contains("synthetic-transport"));
    }
}
