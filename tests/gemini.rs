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

/// A fake provider (named by argv[0]) for supervised runs. Every launch is
/// logged with its account and arguments. Gemini's first account prints
/// `ASWAP_TEST_NOTICE` and then holds the session open, as a CLI that hit
/// its quota does; anything else prints it and exits cleanly.
const FAKE_PROVIDER: &str = r##"import json, os, pathlib, sys, time
provider=pathlib.Path(sys.argv[0]).name
args=sys.argv[1:]
if provider=='gemini':
    home=pathlib.Path(os.environ['GEMINI_CLI_HOME'])/'.gemini'
    auth=json.loads((home/'oauth_creds.json').read_text())['access_token']
else:
    home=pathlib.Path(os.environ['CODEX_HOME'])
    if args[:1]==['-c']: args=args[2:]
    if args[:1]==['app-server']:
        # Codex's model list, so both synthetic accounts can serve the model.
        for line in sys.stdin:
            req=json.loads(line)
            if req.get('method')=='initialize': result={}
            elif req.get('method')=='model/list': result={'data':[{'model':'synthetic-model','id':'synthetic-model','isDefault':True}]}
            else: continue
            print(json.dumps({'id':req['id'],'result':result}),flush=True)
        sys.exit(0)
    auth=json.loads((home/'auth.json').read_text())['tokens']['access_token']
log=pathlib.Path(os.environ['ASWAP_TEST_LAUNCHES'])
with log.open('a') as f: f.write(json.dumps({'provider':provider,'auth':auth,'args':args})+'\n')
assert os.isatty(1)
if provider=='gemini' and '--resume' in args: sys.exit(0)
print(os.environ['ASWAP_TEST_NOTICE'],flush=True)
if provider=='gemini' and auth=='synthetic-g-one':
    time.sleep(30)
    sys.exit(9)
time.sleep(.5)
sys.exit(0)
"##;

fn supervised(provider: Provider, notice: &str) -> (Vec<Value>, String) {
    let t = tempfile::tempdir().unwrap();
    let cli = fake_install(t.path());
    let bin = t.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let python = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|p| p.join("python3"))
        .find(|p| p.is_file())
        .unwrap();
    // The Gemini CLI install itself is the fake, so its client still reads.
    for path in [cli.clone(), bin.join("codex")] {
        fs::write(&path, format!("#!{}\n{FAKE_PROVIDER}", python.display())).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let path = std::env::join_paths(
        std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let launches = t.path().join("launches.jsonl");
    let project = t.path().join("project");
    fs::create_dir(&project).unwrap();
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "supervised_child", "--nocapture"])
        .current_dir(&project)
        .env_clear()
        .env("PATH", path)
        .env("HOME", t.path().join("home"))
        .env("XDG_DATA_HOME", t.path().join("data"))
        .env("XDG_CONFIG_HOME", t.path().join("config"))
        .env("CODEX_HOME", t.path().join("codex"))
        .env("ASWAP_GEMINI_CHILD", "supervised_child")
        .env("ASWAP_GEMINI_BIN", &cli)
        .env("ASWAP_TEST_PROVIDER", provider.as_str())
        .env("ASWAP_TEST_NOTICE", notice)
        .env("ASWAP_TEST_LAUNCHES", &launches)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let launches = fs::read_to_string(&launches)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    (launches, String::from_utf8_lossy(&out.stderr).into_owned())
}

#[test]
fn supervised_child() {
    if !is_child("supervised_child") {
        return;
    }
    use aswap::usage::{Cached, Usage, Window};
    let store = Store::from_env().unwrap();
    let provider = match std::env::var("ASWAP_TEST_PROVIDER").unwrap().as_str() {
        "gemini" => Provider::Gemini,
        _ => Provider::Codex,
    };
    let home = provider.current_home().unwrap();
    fs::create_dir_all(&home).unwrap();
    // Far-off expiry and a fresh usage cache: nothing is refreshed or fetched.
    let expiry = (aswap::now() + 86_400) * 1000;
    for (label, used) in [("one", 20.0), ("two", 10.0)] {
        if provider == Provider::Gemini {
            fs::create_dir_all(home.join("tmp")).unwrap();
            store::json_write(
                &home.join("oauth_creds.json"),
                &login(expiry, &format!("synthetic-g-{label}")),
            )
            .unwrap();
            store::json_write(
                &home.join("google_accounts.json"),
                &json!({"active":format!("{label}@example.com"),"old":[]}),
            )
            .unwrap();
        } else {
            store::json_write(
                &home.join("auth.json"),
                &json!({"tokens":{"access_token":format!("synthetic-g-{label}")}}),
            )
            .unwrap();
        }
        provider::add(&store, provider, label).unwrap();
        let cached = Cached {
            usage: Usage {
                windows: vec![Window {
                    name: "gemini-2.5-pro".into(),
                    used_percent: used,
                    resets_at: None,
                    duration_minutes: Some(1440),
                    codex_slot: None,
                }],
                ..Default::default()
            },
            fetched_at: aswap::now(),
            last_attempt: aswap::now() + 3600,
            next_poll_at: aswap::now() + 3600,
            ..Default::default()
        };
        store::json_write(
            &store
                .profile(provider, label)
                .unwrap()
                .join("usage-cache.json"),
            &cached,
        )
        .unwrap();
    }
    let args = match provider {
        Provider::Codex => vec!["--model".to_owned(), "synthetic-model".to_owned()],
        _ => Vec::new(),
    };
    let code = aswap::runner::run(&store, provider, Some("one"), &args, true).unwrap();
    assert_eq!(code, 0);
}

#[test]
fn a_gemini_limit_notice_fails_over_and_resumes_the_same_session() {
    for notice in [
        "Usage limit reached for gemini-2.5-pro.",
        "You have exhausted your daily quota on this model.",
    ] {
        let (launches, _) = supervised(Provider::Gemini, notice);
        assert_eq!(launches.len(), 2, "{notice}: {launches:?}");
        assert_eq!(launches[0]["auth"], "synthetic-g-one");
        assert_eq!(launches[1]["auth"], "synthetic-g-two", "{notice}");
        let first: Vec<String> = serde_json::from_value(launches[0]["args"].clone()).unwrap();
        let second: Vec<String> = serde_json::from_value(launches[1]["args"].clone()).unwrap();
        let at = first.iter().position(|a| a == "--session-id").unwrap();
        assert_eq!(second[..2], ["--resume".to_owned(), first[at + 1].clone()]);
    }
}

#[test]
fn a_codex_terminal_ignores_geminis_limit_notices() {
    for notice in [
        "Usage limit reached for gemini-2.5-pro.",
        "You have exhausted your daily quota on this model.",
    ] {
        let (launches, stderr) = supervised(Provider::Codex, notice);
        assert!(!stderr.contains("usage limit"), "{notice}: {stderr}");
        assert_eq!(launches.len(), 1, "{notice}: {launches:?}");
        assert_eq!(launches[0]["auth"], "synthetic-g-one");
    }
}
