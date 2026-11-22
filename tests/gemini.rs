//! Gemini accounts in the pool: capture, launch views, refresh write-back,
//! session arguments, and Claude/Codex behaviour unchanged. Synthetic logins
//! only; no network.
use aswap::{
    provider::{self, Provider},
    session::AutoArgs,
    shared::View,
    store::{self, Store},
};
use serde_json::{Value, json};
use std::{fs, path::Path, process::Command};

fn login(expiry_ms: u64, access: &str) -> Value {
    json!({"access_token":access,"refresh_token":"synthetic-g-refresh","expiry_date":expiry_ms,"token_type":"Bearer","scope":"openid"})
}
/// A fake Gemini CLI install (nixpkgs layout) whose bundle assigns a
/// synthetic OAuth client, as the real one assigns its published client.
fn fake_install(root: &Path) -> std::path::PathBuf {
    let share = root.join("cli/share/gemini-cli");
    fs::create_dir_all(&share).unwrap();
    fs::create_dir_all(root.join("cli/bin")).unwrap();
    fs::write(share.join("gemini.js"), "import './chunk-a.js';\n").unwrap();
    let secret = ["GOCSPX", "-synthetic_secret_value"].concat();
    fs::write(
        share.join("chunk-a.js"),
        format!(
            "var OAUTH_CLIENT_ID = \"123456-synthetic.apps.googleusercontent.com\";\nvar OAUTH_CLIENT_SECRET = \"{secret}\";\n"
        ),
    )
    .unwrap();
    let bin = root.join("cli/bin/gemini");
    fs::write(
        &bin,
        format!(
            "#!/bin/sh\nexec node {} \"$@\"\n",
            share.join("gemini.js").display()
        ),
    )
    .unwrap();
    bin
}
fn child(name: &str, t: &Path) {
    let cli = fake_install(t);
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("HOME", t.join("home"))
        .env("XDG_DATA_HOME", t.join("data"))
        .env("XDG_CONFIG_HOME", t.join("config"))
        .env("ASWAP_GEMINI_CHILD", name)
        .env("ASWAP_GEMINI_BIN", &cli)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
fn is_child(name: &str) -> bool {
    std::env::var("ASWAP_GEMINI_CHILD").as_deref() == Ok(name)
}
/// A signed-in `~/.gemini` with a captured profile `label`.
fn captured(label: &str) -> (Store, provider::Provider, std::path::PathBuf) {
    let store = Store::from_env().unwrap();
    let home = Provider::Gemini.current_home().unwrap();
    fs::create_dir_all(home.join("tmp")).unwrap();
    store::json_write(
        &home.join("oauth_creds.json"),
        &login(1_000_000, "synthetic-g-access"),
    )
    .unwrap();
    store::json_write(
        &home.join("google_accounts.json"),
        &json!({"active":"synthetic@example.com","old":[]}),
    )
    .unwrap();
    fs::write(home.join("settings.json"), b"{}").unwrap();
    provider::add(&store, Provider::Gemini, label).unwrap();
    (store, Provider::Gemini, home)
}

#[test]
fn capture_takes_the_google_login_and_refuses_billed_ones() {
    let t = tempfile::tempdir().unwrap();
    child("capture_child", t.path());
}
#[test]
fn capture_child() {
    if !is_child("capture_child") {
        return;
    }
    let (store, provider, home) = captured("personal");
    let profile = store.profile(provider, "personal").unwrap();
    assert_eq!(
        store::json_read::<Value>(&profile.join("oauth_creds.json")).unwrap(),
        login(1_000_000, "synthetic-g-access")
    );
    assert!(profile.join("google_accounts.json").is_file());
    // An API key or a service account is billed: never pooled.
    for billed in [
        json!({"GEMINI_API_KEY":"synthetic-key"}),
        json!({"type":"service_account","private_key":"synthetic"}),
        json!({"access_token":"synthetic-g-access"}),
    ] {
        store::json_write(&home.join("oauth_creds.json"), &billed).unwrap();
        assert!(provider::add(&store, provider, "billed").is_err());
    }
    let state = store.load().unwrap();
    assert_eq!(
        state.active.get(&Provider::Gemini).map(String::as_str),
        Some("personal")
    );
    // list --json: the same schema, provider "gemini", and no secrets.
    let out = Command::new(env!("CARGO_BIN_EXE_aswap"))
        .args(["list", "--json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["schema_version"], 1);
    let row = &v["accounts"][0];
    assert_eq!(row["provider"], "gemini");
    assert_eq!(row["label"], "personal");
    assert_eq!(row["usage_reason"], "never-fetched");
    let text = out.stdout.iter().map(|&b| b as char).collect::<String>();
    assert!(!text.contains("synthetic-g-") && !text.contains("synthetic@example.com"));
}

#[test]
fn a_view_is_private_shares_history_and_writes_back_a_newer_login() {
    let t = tempfile::tempdir().unwrap();
    child("view_child", t.path());
}
#[test]
fn view_child() {
    if !is_child("view_child") {
        return;
    }
    let (store, provider, home) = captured("personal");
    let account = store
        .account(&store.load().unwrap(), provider, Some("personal"))
        .unwrap();
    let mut view = View::prepare(&store, &account).unwrap();
    // GEMINI_CLI_HOME=<view>/.aswap-home, whose .gemini is the view.
    let shim = view.path.join(".aswap-home");
    assert_eq!(fs::read_link(shim.join(".gemini")).unwrap(), view.path);
    // Login files are the view's own; sessions and settings are shared.
    for private in ["oauth_creds.json", "google_accounts.json"] {
        assert!(
            !fs::symlink_metadata(view.path.join(private))
                .unwrap()
                .is_symlink(),
            "{private}"
        );
    }
    assert_eq!(
        fs::read_link(view.path.join("tmp")).unwrap(),
        home.join("tmp")
    );
    assert_eq!(
        fs::read_link(view.path.join("extensions")).unwrap(),
        home.join("extensions")
    );
    assert_eq!(
        fs::read_link(view.path.join("settings.json")).unwrap(),
        home.join("settings.json")
    );
    // The view carries the whole login: Gemini CLI refreshes it itself.
    let in_view: Value = store::json_read(&view.path.join("oauth_creds.json")).unwrap();
    assert_eq!(in_view["refresh_token"], "synthetic-g-refresh");

    // Gemini CLI refreshed in the view: the profile adopts the newer login.
    let profile = store
        .profile(provider, "personal")
        .unwrap()
        .join("oauth_creds.json");
    store::json_write(
        &view.path.join("oauth_creds.json"),
        &login(5_000_000, "synthetic-g-fresh"),
    )
    .unwrap();
    view.sync(&store, &account).unwrap();
    assert_eq!(
        store::json_read::<Value>(&profile).unwrap()["access_token"],
        "synthetic-g-fresh"
    );
    assert_eq!(
        store::json_read::<Value>(&view.path.join("oauth_creds.json")).unwrap()["access_token"],
        "synthetic-g-fresh"
    );
    // An older login in the view never rolls the profile back.
    store::json_write(
        &view.path.join("oauth_creds.json"),
        &login(2_000_000, "synthetic-g-stale"),
    )
    .unwrap();
    view.sync(&store, &account).unwrap();
    assert_eq!(
        store::json_read::<Value>(&profile).unwrap()["access_token"],
        "synthetic-g-fresh"
    );
    assert_eq!(
        store::json_read::<Value>(&view.path.join("oauth_creds.json")).unwrap()["access_token"],
        "synthetic-g-fresh"
    );
    // Nothing private was published to the shared home.
    assert!(!home.join(".aswap-home").exists());
    assert_eq!(
        store::json_read::<Value>(&home.join("oauth_creds.json")).unwrap()["access_token"],
        "synthetic-g-access"
    );
}

#[test]
fn the_owner_refreshes_a_gemini_login_through_googles_form() {
    let t = tempfile::tempdir().unwrap();
    child("refresh_child", t.path());
}
#[test]
fn refresh_child() {
    if !is_child("refresh_child") {
        return;
    }
    let (store, provider, _home) = captured("personal");
    aswap::coordination::owner(&store).unwrap();
    let account = store
        .account(&store.load().unwrap(), provider, Some("personal"))
        .unwrap();
    let _lock = store.lock().unwrap();
    let at = 10_000;
    let refreshed = aswap::refresh::refresh_with(&store, &account, at, false, |p, body| {
        assert_eq!(p, Provider::Gemini);
        assert_eq!(body["grant_type"], "refresh_token");
        assert_eq!(body["refresh_token"], "synthetic-g-refresh");
        // The installed CLI's client, read from its bundle.
        assert_eq!(
            body["client_id"],
            "123456-synthetic.apps.googleusercontent.com"
        );
        assert!(
            body["client_secret"]
                .as_str()
                .unwrap()
                .ends_with("-synthetic_secret_value")
        );
        Ok(json!({"access_token":"synthetic-g-new","expires_in":3599}))
    })
    .unwrap();
    assert!(refreshed);
    let profile = store
        .profile(provider, "personal")
        .unwrap()
        .join("oauth_creds.json");
    let saved: Value = store::json_read(&profile).unwrap();
    assert_eq!(saved["access_token"], "synthetic-g-new");
    assert_eq!(saved["expiry_date"], (at + 3599) * 1000);
    assert_eq!(saved["refresh_token"], "synthetic-g-refresh");
}

#[test]
fn a_gemini_session_is_named_up_front_and_resumed_by_id() {
    let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let parsed = AutoArgs::parse(Provider::Gemini, &args(&["-m", "gemini-2.5-pro"])).unwrap();
    let id = parsed.id.clone().expect("a new session gets an id");
    assert!(aswap::session::valid_id(&id));
    assert_eq!(
        &parsed.initial[2..4],
        ["--session-id".to_owned(), id.clone()]
    );
    let resume = parsed.resume(Provider::Gemini, &id, None).unwrap();
    assert_eq!(resume[..2], ["--resume".to_owned(), id.clone()]);
    assert!(resume.contains(&"-m".to_owned()));
    // A user-chosen session keeps its own; "latest" is not an exact id.
    let latest = AutoArgs::parse(Provider::Gemini, &args(&["--resume", "latest"])).unwrap();
    assert!(latest.id.is_none());
    assert!(!latest.initial.contains(&"--session-id".to_owned()));
    let exact = "11111111-2222-4333-8444-555555555555";
    let named = AutoArgs::parse(Provider::Gemini, &args(&["--resume", exact])).unwrap();
    assert_eq!(named.id.as_deref(), Some(exact));
    // A non-interactive prompt is a one-shot.
    assert!(
        AutoArgs::parse(Provider::Gemini, &args(&["-p", "hi"]))
            .unwrap()
            .one_shot
    );
}

#[test]
fn a_gemini_launch_is_isolated_and_claude_and_codex_launches_are_unchanged() {
    let view = Path::new("/synthetic/runtimes/view");
    let env = |cmd: &Command, key: &str| {
        cmd.get_envs()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.map(|v| v.to_string_lossy().into_owned()))
    };
    let gemini = Provider::Gemini.command(view);
    assert_eq!(
        env(&gemini, "GEMINI_CLI_HOME"),
        Some(Some("/synthetic/runtimes/view/.aswap-home".into()))
    );
    for key in [
        "GEMINI_API_KEY",
        "GOOGLE_APPLICATION_CREDENTIALS",
        "GEMINI_FORCE_ENCRYPTED_FILE_STORAGE",
    ] {
        assert_eq!(env(&gemini, key), Some(None), "{key} is scrubbed");
    }
    // Claude and Codex: their own variable on the view, no Gemini ones.
    for (p, var) in [
        (Provider::Claude, "CLAUDE_CONFIG_DIR"),
        (Provider::Codex, "CODEX_HOME"),
    ] {
        let cmd = p.command(view);
        assert_eq!(
            env(&cmd, var),
            Some(Some(view.to_string_lossy().into_owned()))
        );
        assert_eq!(env(&cmd, "GEMINI_CLI_HOME"), None);
        assert_eq!(
            env(&cmd, "GEMINI_API_KEY"),
            None,
            "{p} doesn't touch Gemini's variables"
        );
    }
    assert_eq!(
        Provider::Codex.command(view).get_args().collect::<Vec<_>>(),
        ["-c", "cli_auth_credentials_store=\"file\""]
    );
    assert_eq!(Provider::Gemini.command(view).get_args().count(), 0);
}
