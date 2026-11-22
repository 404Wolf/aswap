use aswap::{
    provider::Provider,
    store::{self, Account, Store},
};
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
};
fn account(name: &str) -> Account {
    Account {
        provider: Provider::Claude,
        label: name.into(),
    }
}
#[test]
fn private_atomic_storage_and_validation() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("store");
    let store = Store::open(root.clone()).unwrap();
    let _lock = store.lock().unwrap();
    let mut state = store.load().unwrap();
    state.accounts.push(account("work"));
    store.save(&state).unwrap();
    assert_eq!(
        fs::metadata(&root).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(root.join("state.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(store.load().unwrap().accounts[0].label, "work");
    for bad in [
        "../escape",
        "",
        "a/b",
        "person@example.com",
        "<span>",
        "a\nb",
    ] {
        assert!(store.profile(Provider::Claude, bad).is_err());
    }
    assert!(store::threshold(f64::NAN).is_err());
    assert!(store::threshold(101.0).is_err());
}
#[test]
fn reject_symlinks_and_repository_storage() {
    let t = tempfile::tempdir().unwrap();
    let dest = t.path().join("dest");
    fs::create_dir(&dest).unwrap();
    let link = t.path().join("link");
    symlink(&dest, &link).unwrap();
    assert!(Store::open(link.join("child")).is_err());
    let path = dest.join("file");
    symlink("/dev/null", &path).unwrap();
    assert!(store::atomic(&path, b"synthetic").is_err());
    assert!(store::read_private(&path).is_err());
    fs::create_dir(dest.join(".git")).unwrap();
    assert!(Store::open(dest.join("store")).is_err());
}
#[test]
fn leases_prevent_parallel_mutation_of_same_profile() {
    let t = tempfile::tempdir().unwrap();
    let store = Store::open(t.path().join("store")).unwrap();
    let a = account("a");
    let _lease = store.lease(&a).unwrap();
    assert!(store.lease(&a).is_ok());
    assert!(store.lease(&account("b")).is_ok());
}
#[test]
fn parse_errors_never_echo_private_content() {
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("private");
    store::atomic(&path, b"synthetic-private-bad-json").unwrap();
    let err = store::json_read::<serde_json::Value>(&path).unwrap_err();
    assert!(!format!("{err:?} {err}").contains("synthetic-private"));
}

#[test]
fn lifecycle_in_isolated_child() {
    let t = tempfile::tempdir().unwrap();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "isolated_lifecycle_child", "--nocapture"])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("HOME", t.path())
        .env("XDG_DATA_HOME", t.path().join("data"))
        .env("CLAUDE_CONFIG_DIR", t.path().join("claude"))
        .env("CODEX_HOME", t.path().join("codex"))
        .env("ASWAP_SYNTHETIC_CHILD", "1")
        .status()
        .unwrap();
    assert!(status.success());
}
#[test]
fn isolated_lifecycle_child() {
    if std::env::var("ASWAP_SYNTHETIC_CHILD").as_deref() != Ok("1") {
        return;
    }
    let store = Store::from_env().unwrap();
    let claude = Provider::Claude.current_home().unwrap();
    store::atomic(&claude.join(".credentials.json"), br#"{"claudeAiOauth":{"accessToken":"synthetic-first","refreshToken":"synthetic-refresh"}}"#).unwrap();
    store::atomic(
        &claude.join(".claude.json"),
        br#"{"oauthAccount":{"emailAddress":"one@example.com"},"projects":{"synthetic":true}}"#,
    )
    .unwrap();
    aswap::provider::add(&store, Provider::Claude, "one").unwrap();
    assert!(aswap::provider::add(&store, Provider::Claude, "one").is_err());
    store::atomic(
        &claude.join(".credentials.json"),
        br#"{"claudeAiOauth":{"accessToken":"synthetic-second"}}"#,
    )
    .unwrap();
    store::atomic(
        &claude.join(".claude.json"),
        br#"{"oauthAccount":{"emailAddress":"two@example.com"},"projects":{"synthetic":true}}"#,
    )
    .unwrap();
    aswap::provider::add(&store, Provider::Claude, "two").unwrap();
    // Rotate the outgoing live token after capture; activation must preserve it.
    store::atomic(
        &claude.join(".credentials.json"),
        br#"{"claudeAiOauth":{"accessToken":"synthetic-second-rotated"}}"#,
    )
    .unwrap();
    aswap::provider::switch(&store, Provider::Claude, "one", true).unwrap();
    let outgoing = store::read_private(
        &store
            .profile(Provider::Claude, "two")
            .unwrap()
            .join(".credentials.json"),
    )
    .unwrap();
    assert!(
        String::from_utf8(outgoing)
            .unwrap()
            .contains("synthetic-second-rotated")
    );
    assert_eq!(
        std::fs::read_dir(store.root.join("live-backups/claude"))
            .unwrap()
            .count(),
        1
    );

    let config: serde_json::Value = store::json_read(&claude.join(".claude.json")).unwrap();
    assert_eq!(config["projects"]["synthetic"], true);
    assert_eq!(config["oauthAccount"]["emailAddress"], "one@example.com");
    let bytes = store::read_private(&claude.join(".credentials.json")).unwrap();
    assert!(
        String::from_utf8(bytes.clone())
            .unwrap()
            .contains("synthetic-first")
    );
    store::atomic(&claude.join(".claude.json"), b"invalid synthetic-private").unwrap();
    assert!(aswap::provider::switch(&store, Provider::Claude, "two", true).is_err());
    assert_eq!(
        bytes,
        store::read_private(&claude.join(".credentials.json")).unwrap()
    );
    let codex = Provider::Codex.current_home().unwrap();
    store::atomic(
        &codex.join("auth.json"),
        br#"{"tokens":{"access_token":"synthetic-codex"}}"#,
    )
    .unwrap();
    aswap::provider::add(&store, Provider::Codex, "work").unwrap();
    aswap::provider::switch(&store, Provider::Codex, "work", false).unwrap();
    let a = account("one");
    let lease = store.lease(&a).unwrap();
    assert!(aswap::provider::remove(&store, Provider::Claude, "one").is_err());
    drop(lease);
    aswap::provider::remove(&store, Provider::Claude, "one").unwrap();
    assert!(!store.profile(Provider::Claude, "one").unwrap().exists());
    assert!(!store.load().unwrap().active.contains_key(&Provider::Claude));
}

#[test]
fn legacy_auto_settings_get_default_prompt_and_off_survives_reload() {
    let mut policy: store::Auto =
        serde_json::from_value(serde_json::json!({"enabled":true,"threshold":90,"last_switch":0}))
            .unwrap();
    assert_eq!(policy.resume_prompt.as_deref(), Some(store::RESUME_PROMPT));
    policy.resume_prompt = None;
    let restored: store::Auto =
        serde_json::from_value(serde_json::to_value(policy).unwrap()).unwrap();
    assert!(restored.resume_prompt.is_none());
}
#[test]
fn legacy_auto_settings_load_with_no_reservations() {
    let temp = tempfile::tempdir().unwrap();
    let store = aswap::store::Store::open(temp.path().join("store")).unwrap();
    aswap::store::json_write(
        &store.root.join("state.json"),
        &serde_json::json!({
            "accounts": [],
            "active": {},
            "auto": {"claude": {"enabled":true,"threshold":85.0,"last_switch":10}}
        }),
    )
    .unwrap();
    let state = store.load().unwrap();
    let settings = &state.auto[&aswap::provider::Provider::Claude];
    assert!(settings.enabled);
    assert!(settings.reserved.is_empty());
}

/// state.json as a newer aswap might write it: a provider this version
/// doesn't know (its account, active label and auto settings), a new
/// top-level field and a new Auto field.
const NEWER: &str = r#"{
  "accounts": [
    {"provider": "claude", "label": "personal"},
    {"provider": "agy", "label": "personal", "since": "0.3.0"},
    {"provider": "codex", "label": "work"}
  ],
  "active": {"claude": "personal", "agy": "personal"},
  "auto": {
    "claude": {"resume_prompt": null, "enabled": true, "threshold": 90.0, "last_switch": 5,
               "reserved": ["work"], "future_knob": {"level": 3}},
    "agy": {"enabled": true, "threshold": 80.0, "last_switch": 0, "quota_groups": ["flash"]}
  },
  "switch_interval_seconds": 7,
  "future_top_level": [1, 2, 3]
}"#;

#[test]
fn state_written_by_a_newer_version_is_read_and_kept_whole() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("store")).unwrap();
    let path = temp.path().join("store/state.json");
    store::atomic(&path, NEWER.as_bytes()).unwrap();
    let mut state = store.load().unwrap();
    // This version sees its own providers, settings and fields...
    let labels: Vec<_> = state
        .accounts
        .iter()
        .map(|a| format!("{}:{}", a.provider, a.label))
        .collect();
    assert_eq!(labels, ["claude:personal", "codex:work"]);
    assert_eq!(
        state.active.get(&Provider::Claude).map(String::as_str),
        Some("personal")
    );
    let claude = &state.auto[&Provider::Claude];
    assert!(claude.enabled && claude.reserved == ["work"]);
    assert_eq!(state.switch_interval_seconds, 7);
    // ...changes one, and saves: nothing it doesn't know is lost.
    state.auto.get_mut(&Provider::Claude).unwrap().last_switch = 99;
    store.save(&state).unwrap();
    let saved: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let mut expected: serde_json::Value = serde_json::from_str(NEWER).unwrap();
    expected["auto"]["claude"]["last_switch"] = 99.into();
    for (key, value) in serde_json::to_value(&state.auto[&Provider::Claude])
        .unwrap()
        .as_object()
        .unwrap()
    {
        // Defaults this version fills in for fields the file left out.
        expected["auto"]["claude"]
            .as_object_mut()
            .unwrap()
            .entry(key.clone())
            .or_insert(value.clone());
    }
    let set = |v: &serde_json::Value| {
        let mut accounts: Vec<String> = v["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a.to_string())
            .collect();
        accounts.sort();
        accounts
    };
    assert_eq!(set(&saved), set(&expected));
    for key in [
        "active",
        "auto",
        "switch_interval_seconds",
        "future_top_level",
    ] {
        assert_eq!(saved[key], expected[key], "{key}");
    }
}

#[test]
fn a_known_provider_with_bad_settings_is_still_an_error() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("store")).unwrap();
    let path = temp.path().join("store/state.json");
    for bad in [
        r#"{"accounts":[{"provider":"claude"}],"active":{},"auto":{}}"#,
        r#"{"accounts":[],"active":{"codex":7},"auto":{}}"#,
        r#"{"accounts":[],"active":{},"auto":{"claude":{"enabled":"yes"}}}"#,
    ] {
        store::atomic(&path, bad.as_bytes()).unwrap();
        assert!(store.load().is_err(), "{bad}");
    }
    // An older file without the newer fields still loads.
    store::atomic(&path, br#"{"accounts":[],"active":{},"auto":{}}"#).unwrap();
    assert!(store.load().unwrap().switch_interval_seconds > 0);
}
