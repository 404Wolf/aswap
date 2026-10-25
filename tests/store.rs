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
    assert!(store.lease(&a).is_err());
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
