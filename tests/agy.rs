//! agy (Antigravity CLI) accounts: capture, an isolated launch, failover on
//! native quota observations, and login write-back.
//! Synthetic logins and a fake agy only; no network.
use aswap::{
    provider::{self, Provider},
    store::{self, Store},
};
use serde_json::{Value, json};
use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command};

const ID: &str = "666838c2-f624-4ff9-9039-82e50ca540cf";

fn login(email: &str, expiry: &str) -> Value {
    json!({"token":{"access_token":format!("synthetic-a-{email}"),"token_type":"Bearer",
        "refresh_token":format!("synthetic-r-{email}"),"expiry":expiry,"expires_in":3599},
        "auth_method":"consumer","id_token_email":email})
}

fn token_path(home: &Path) -> std::path::PathBuf {
    home.join(".gemini/antigravity-cli/antigravity-oauth-token")
}

/// A fake agy: checks its isolation, holds presence/<id>.lock as agy does,
/// and logs each launch. `ASWAP_TEST_MODE`:
/// - partial/all/offline: account one prints a quota notice and exits;
/// - redraw: account one prints the notice, then keeps working;
/// - refresh: account one writes a refreshed login (later expiry), exits.
const FAKE_AGY: &str = r##"import json, os, pathlib, sys, time
args=sys.argv[1:]
root=pathlib.Path(next(a.split('=',1)[1] for a in args if a.startswith('--gemini_dir=')))
args=[a for a in args if not a.startswith('--gemini_dir=')]
assert not os.environ.keys() & {
    'DBUS_SESSION_BUS_ADDRESS', 'GEMINI_CLI_HOME', 'GEMINI_API_KEY', 'GOOGLE_API_KEY',
    'GOOGLE_GEMINI_BASE_URL', 'GOOGLE_APPLICATION_CREDENTIALS', 'GOOGLE_GENAI_USE_VERTEXAI',
    'GOOGLE_CLOUD_PROJECT', 'GOOGLE_CLOUD_QUOTA_PROJECT', 'GOOGLE_CLOUD_LOCATION', 'AGY_ADC_AUTH',
    'AGY_LLM_GATEWAY_URL', 'AGY_LLM_GATEWAY_API_KEY', 'AGY_LLM_GATEWAY_HEADERS',
    'AGY_LLM_GATEWAY_MODELS', 'AGY_LLM_GATEWAY_CA_CERT', 'AGY_LLM_GATEWAY_PROXY_URL',
}
control=pathlib.Path(__file__).parent.parent
if args==['--version']:
    print('1.5.13'); sys.exit(0)
if args==['-p','/usage','--output-format','json']:
    mode=(control/'mode').read_text()
    with (control/'usage-reads').open('a') as f: f.write('quota\n')
    if mode=='offline': sys.exit(7)
    buckets=[{'id':'gemini-weekly','window':'weekly','remaining_fraction':0,
              'reset_time':'2099-01-01T00:00:00Z'}]
    if mode=='all':
        buckets.append({'id':'3p-5h','window':'5h','remaining_fraction':0,
                        'reset_time':'2099-01-01T00:00:00Z'})
    print(json.dumps({'status':'SUCCESS','conversation_id':'','num_turns':0,
                      'command':{'name':'usage','data':{'groups':[{'buckets':buckets}]}}}))
    sys.exit(0)
if '--settings' in args:
    override=pathlib.Path(args[args.index('--settings')+1])
    assert override.name.startswith('aswap-agy-settings-')
    assert json.loads(override.read_text())=={'useG1Credits':False,'modelProvider':''}
config=pathlib.Path(os.environ['XDG_CONFIG_HOME'])
assert config==root/'.config'
# Tools agy runs see the user's config (gh here), but agy's own secrets
# are the view's.
assert (config/'gh'/'hosts.yml').read_text()=='synthetic: gh\n'
assert not (config/'antigravity'/'plugin-secrets.json').exists()
assert (root/'antigravity-cli'/'jetski_state.pbtxt').is_file() and not (root/'antigravity-cli'/'jetski_state.pbtxt').is_symlink()
assert os.environ['AGY_CLI_DISABLE_AUTO_UPDATE']=='true'
app=root/'antigravity-cli'
token=app/'antigravity-oauth-token'
login=json.loads(token.read_text())
who=login['id_token_email'].split('@')[0]
mode=os.environ['ASWAP_TEST_MODE']
log=pathlib.Path(os.environ['ASWAP_TEST_LAUNCHES'])
with log.open('a') as f: f.write(json.dumps({'account':who,'args':args})+'\n')
# Shared state is linked in, the login and settings are the view's own.
assert (app/'brain').is_symlink() and (app/'presence').is_symlink() and (root/'config').is_symlink()
assert not token.is_symlink()
if '--conversation' in args: sys.exit(0)
(app/'presence').mkdir(exist_ok=True)
held=open(app/'presence'/(os.environ['ASWAP_TEST_ID']+'.lock'),'w')
transcript=app/'brain'/os.environ['ASWAP_TEST_ID']/'.system_generated'/'logs'/'transcript.jsonl'
transcript.parent.mkdir(parents=True,exist_ok=True)
transcript.write_text('{"type":"user"}\n')
if who!='one': sys.exit(0)
if mode=='refresh':
    login['token']['expiry']='2099-01-01T00:00:00Z'
    login['token']['access_token']='synthetic-a-refreshed'
    with token.open('w') as f: f.write(json.dumps(login))
    time.sleep(2.5)
    sys.exit(0)
if mode=='launch': sys.exit(0)
print('Quota exhausted', flush=True)
if mode in ('partial', 'all', 'offline'): sys.exit(9)
for _ in range(3):
    time.sleep(1)
    step={'type':'PLANNER_RESPONSE','status':'DONE','source':'MODEL',
          'created_at':__import__('datetime').datetime.now(__import__('datetime').timezone.utc).isoformat()}
    with transcript.open('a') as f: f.write(json.dumps(step)+'\n')
sys.exit(0)
"##;

fn child(name: &str, mode: &str) -> (Vec<Value>, std::path::PathBuf, tempfile::TempDir, String) {
    child_with(name, mode, "[]")
}
fn child_with(
    name: &str,
    mode: &str,
    args: &str,
) -> (Vec<Value>, std::path::PathBuf, tempfile::TempDir, String) {
    let t = tempfile::tempdir().unwrap();
    fs::write(t.path().join("mode"), mode).unwrap();
    let bin = t.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let python = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|p| p.join("python3"))
        .find(|p| p.is_file())
        .unwrap();
    let agy = bin.join("agy");
    fs::write(&agy, format!("#!{}\n{FAKE_AGY}", python.display())).unwrap();
    fs::set_permissions(&agy, fs::Permissions::from_mode(0o700)).unwrap();
    let launches = t.path().join("launches.jsonl");
    let project = t.path().join("project");
    fs::create_dir(&project).unwrap();
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .current_dir(&project)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("HOME", t.path().join("home"))
        .env("XDG_DATA_HOME", t.path().join("data"))
        .env("XDG_CONFIG_HOME", t.path().join("config"))
        .envs(
            [
                "DBUS_SESSION_BUS_ADDRESS",
                "GEMINI_CLI_HOME",
                "GEMINI_API_KEY",
                "GOOGLE_API_KEY",
                "GOOGLE_GEMINI_BASE_URL",
                "GOOGLE_APPLICATION_CREDENTIALS",
                "GOOGLE_GENAI_USE_VERTEXAI",
                "GOOGLE_CLOUD_PROJECT",
                "GOOGLE_CLOUD_QUOTA_PROJECT",
                "GOOGLE_CLOUD_LOCATION",
                "AGY_ADC_AUTH",
                "AGY_LLM_GATEWAY_URL",
                "AGY_LLM_GATEWAY_API_KEY",
                "AGY_LLM_GATEWAY_HEADERS",
                "AGY_LLM_GATEWAY_MODELS",
                "AGY_LLM_GATEWAY_CA_CERT",
                "AGY_LLM_GATEWAY_PROXY_URL",
            ]
            .map(|key| (key, "synthetic-alternate")),
        )
        .env("ASWAP_AGY_BIN", &agy)
        .env("ASWAP_AGY_CHILD", name)
        .env("ASWAP_TEST_MODE", mode)
        .env("ASWAP_TEST_ID", ID)
        .env("ASWAP_TEST_LAUNCHES", &launches)
        .env("ASWAP_TEST_ARGS", args)
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
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let home = t.path().join("home");
    (
        launches,
        home,
        t,
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}
fn is_child(name: &str) -> bool {
    std::env::var("ASWAP_AGY_CHILD").as_deref() == Ok(name)
}
/// Two agy accounts, `one` (active) and `two`, captured from ~/.gemini.
fn two_accounts() -> Store {
    let store = Store::from_env().unwrap();
    let home = provider::home().unwrap();
    // The user's own config, and agy's first-run state.
    let config = std::path::PathBuf::from(std::env::var_os("XDG_CONFIG_HOME").unwrap());
    fs::create_dir_all(config.join("gh")).unwrap();
    fs::write(config.join("gh/hosts.yml"), "synthetic: gh\n").unwrap();
    fs::create_dir_all(config.join("antigravity")).unwrap();
    fs::write(config.join("antigravity/plugin-secrets.json"), "{}").unwrap();
    fs::create_dir_all(home.join(".gemini/antigravity-cli")).unwrap();
    fs::write(
        home.join(".gemini/antigravity-cli/jetski_state.pbtxt"),
        "post_onboarding: true\n",
    )
    .unwrap();
    for name in ["one", "two"] {
        store::json_write(
            &token_path(&home),
            &login(&format!("{name}@example.com"), "2098-01-01T00:00:00Z"),
        )
        .unwrap();
        provider::add(&store, Provider::Agy, name).unwrap();
    }
    store
}

#[test]
fn capture_copies_the_login_and_refuses_billed_or_torn_ones() {
    let _ = child("capture_child", "none");
}
#[test]
fn capture_child() {
    if !is_child("capture_child") {
        return;
    }
    let store = Store::from_env().unwrap();
    let home = provider::home().unwrap();
    let source = token_path(&home);
    // A torn read (agy rewrites in place) or a billed login is refused.
    fs::create_dir_all(source.parent().unwrap()).unwrap();
    fs::write(&source, br#"{"token":{"access_tok"#).unwrap();
    assert!(provider::add(&store, Provider::Agy, "personal").is_err());
    let mut billed = login("one@example.com", "2026-11-22T08:00:00Z");
    billed["auth_method"] = "api_key".into();
    store::json_write(&source, &billed).unwrap();
    assert!(provider::add(&store, Provider::Agy, "personal").is_err());
    // A personal login is copied; the original is never written.
    store::json_write(&source, &login("one@example.com", "2026-11-22T08:00:00Z")).unwrap();
    let before = (
        fs::read(&source).unwrap(),
        fs::metadata(&source).unwrap().modified().unwrap(),
    );
    provider::add(&store, Provider::Agy, "personal").unwrap();
    assert_eq!(
        (
            fs::read(&source).unwrap(),
            fs::metadata(&source).unwrap().modified().unwrap()
        ),
        before
    );
    let copy: Value = store::json_read(
        &store
            .profile(Provider::Agy, "personal")
            .unwrap()
            .join(Provider::Agy.auth_file()),
    )
    .unwrap();
    assert_eq!(copy["id_token_email"], "one@example.com");
    // list --json: provider agy, no tokens or address.
    let out = Command::new(env!("CARGO_BIN_EXE_aswap"))
        .args(["list", "--json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["accounts"][0]["provider"], "agy");
    assert!(
        !text.contains("synthetic-") && !text.contains("one@example.com"),
        "{text}"
    );
}

#[test]
fn an_agy_notice_records_only_observed_buckets_and_never_rejects_the_account() {
    for (mode, count) in [("partial", 1), ("all", 2), ("offline", 0)] {
        let (launches, home, temp, stderr) =
            child_with("run_child", mode, r#"["-i","synthetic first prompt"]"#);
        assert_eq!(launches.len(), 1, "{mode}: {launches:?}");
        assert_eq!(launches[0]["account"], "one");
        let first: Vec<String> = serde_json::from_value(launches[0]["args"].clone()).unwrap();
        assert!(first.contains(&"synthetic first prompt".to_owned()));
        assert_eq!(
            stderr.matches("runs without the D-Bus session bus").count(),
            1
        );
        assert_eq!(
            fs::read_to_string(temp.path().join("usage-reads")).unwrap(),
            "quota\n"
        );
        let data = home.parent().unwrap().join("data/aswap/profiles/agy");
        let cache: aswap::usage::Cached =
            store::json_read(&data.join("one/usage-cache.json")).unwrap();
        assert_ne!(cache.error.as_deref(), Some("provider-limit"));
        assert_eq!(cache.usage.windows.len(), count, "{mode}");
        assert!(cache.usage.windows.iter().all(|w| w.used_percent == 100.0));
        assert!(!data.join("two/usage-cache.json").exists());
    }
}

#[test]
fn a_redrawn_agy_notice_followed_by_work_never_switches() {
    let (launches, _, _t, _) = child("run_child", "redraw");
    assert_eq!(launches.len(), 1, "{launches:?}");
}

#[test]
fn cached_agy_buckets_do_not_select_a_different_default_or_launch_account() {
    let (launches, _, temp, _) = child("run_child", "launch");
    assert_eq!(launches.len(), 1);
    assert_eq!(launches[0]["account"], "one");
    assert!(!temp.path().join("usage-reads").exists());
}

#[test]
fn a_login_agy_refreshed_in_its_view_is_written_back() {
    let (launches, home, _t, _) = child("run_child", "refresh");
    assert_eq!(launches.len(), 1, "{launches:?}");
    let data = home.parent().unwrap().join("data/aswap");
    let profile: Value = store::json_read(
        &data
            .join("profiles/agy/one")
            .join(Provider::Agy.auth_file()),
    )
    .unwrap();
    assert_eq!(profile["token"]["expiry"], "2099-01-01T00:00:00Z");
    // The original in ~/.gemini was never touched.
    let original: Value = store::json_read(&token_path(&home)).unwrap();
    assert_eq!(original["id_token_email"], "two@example.com");
    // And the view no longer holds a login once the run is over.
    let views = data.join("runtimes/agy/one");
    for view in fs::read_dir(views).unwrap() {
        assert!(
            !view
                .unwrap()
                .path()
                .join(Provider::Agy.auth_file())
                .exists()
        );
    }
}

#[test]
fn run_child() {
    if !is_child("run_child") {
        return;
    }
    let store = two_accounts();
    let args: Vec<String> =
        serde_json::from_str(&std::env::var("ASWAP_TEST_ARGS").unwrap_or("[]".into())).unwrap();
    let mode = std::env::var("ASWAP_TEST_MODE").unwrap();
    if mode == "launch" {
        let mut state = store.load().unwrap();
        state.active.insert(Provider::Agy, "one".into());
        state.auto.entry(Provider::Agy).or_default().enabled = true;
        store.save(&state).unwrap();
        for (label, percent) in [("one", 100.0), ("two", 5.0)] {
            let cache = aswap::usage::Cached {
                usage: aswap::usage::Usage {
                    windows: vec![aswap::usage::Window {
                        name: "gemini-5h".into(),
                        used_percent: percent,
                        duration_minutes: Some(300),
                        resets_at: Some(aswap::now() + 3600),
                        codex_slot: None,
                    }],
                    ..Default::default()
                },
                fetched_at: aswap::now(),
                next_poll_at: aswap::now() + aswap::usage::TTL,
                ..Default::default()
            };
            let profile = store.profile(Provider::Agy, label).unwrap();
            store::json_write(&profile.join("usage-cache.json"), &cache).unwrap();
        }
        assert!(
            aswap::runner::tick(&store, Provider::Agy, false)
                .unwrap()
                .is_none()
        );
    }
    let pinned = (mode != "launch").then_some("one");
    let code = aswap::runner::run(&store, Provider::Agy, pinned, &args, true).unwrap();
    assert_eq!(
        code,
        if matches!(mode.as_str(), "partial" | "all" | "offline") {
            9
        } else {
            0
        }
    );
}

#[test]
fn options_that_leave_the_managed_personal_sign_in_are_refused() {
    let t = tempfile::tempdir().unwrap();
    let paid = t.path().join("paid.json");
    fs::write(&paid, r#"{"useG1Credits": true}"#).unwrap();
    let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    for refused in [
        args(&["--gemini_dir=/elsewhere"]),
        args(&["-gemini_dir", "/elsewhere"]),
        args(&["--app_data_dir=other"]),
        args(&["--settings", r#"{"useG1Credits":true}"#]),
        args(&["--settings", r#"{"UseG1Credits":true}"#]),
        args(&["--settings", r#"{"u\u017feG1Credits":true}"#]),
        args(&["--settings", r#"{"useG1Credits":true,"useG1Credits":null}"#]),
        args(&["--settings", r#"{"modelProvider":"gemini"}"#]),
        args(&["--settings", r#"{"ModelProvider":"gemini"}"#]),
        args(&["--settings", "[]"]),
        args(&["-settings="]),
        args(&["--settings", paid.to_str().unwrap()]),
    ] {
        assert!(aswap::agy::check_args(&refused).is_err(), "{refused:?}");
    }
    for allowed in [
        args(&["--settings", r#"{"toolPermission":"always-proceed"}"#]),
        args(&["--settings", r#"{"useG1Credits":false,"modelProvider":""}"#]),
        args(&["--model", "synthetic"]),
        args(&["--", "--gemini_dir=/literal/prompt/text"]),
    ] {
        assert!(aswap::agy::check_args(&allowed).is_ok(), "{allowed:?}");
    }
}

#[test]
fn agy_checks_persisted_settings_and_hands_off_the_validated_override_copy() {
    let temporary = tempfile::tempdir().unwrap();
    let app = temporary.path().join(aswap::agy::APP);
    fs::create_dir(&app).unwrap();
    let store = Store::open(temporary.path().join("store")).unwrap();
    let settings = app.join("settings.json");
    assert!(store.command(Provider::Agy, temporary.path()).is_ok());
    fs::write(&settings, r#"{"useG1Credits":false,"modelProvider":""}"#).unwrap();
    assert!(store.command(Provider::Agy, temporary.path()).is_ok());

    let args = vec![format!("--settings={}", settings.display())];
    let checked = aswap::agy::check_args(&args).unwrap();
    let copy = Path::new(checked.args[0].strip_prefix("--settings=").unwrap()).to_owned();
    assert_ne!(copy, settings);
    fs::write(
        &settings,
        r#"{"modelProvider":"gemini","useG1Credits":true}"#,
    )
    .unwrap();
    assert!(store.command(Provider::Agy, temporary.path()).is_err());
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(&copy).unwrap()).unwrap(),
        json!({"useG1Credits":false, "modelProvider":""}),
    );
    drop(checked);
    assert!(!copy.exists());
}

#[test]
fn agy_launch_reads_the_validated_settings_file() {
    let source = tempfile::NamedTempFile::new().unwrap();
    fs::write(
        source.path(),
        r#"{"useG1Credits":false,"modelProvider":""}"#,
    )
    .unwrap();
    let args = json!(["--settings", source.path()]);
    let (launches, _, _, _) = child_with("run_child", "launch", &args.to_string());
    assert_eq!(launches.len(), 1);
    assert_ne!(launches[0]["args"][1], json!(source.path()));
}

#[test]
fn go_style_single_dash_flags_are_read_like_double_dash_ones() {
    use aswap::session::AutoArgs;
    let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let named =
        AutoArgs::parse(Provider::Agy, &args(&["-conversation", ID, "-model", "m"])).unwrap();
    assert_eq!(named.id.as_deref(), Some(ID));
    // -prompt-interactive's text is never kept for a restart.
    let first = AutoArgs::parse(
        Provider::Agy,
        &args(&["-prompt-interactive", "hello", "-model", "m"]),
    )
    .unwrap();
    let resumed = first.resume(Provider::Agy, ID, None).unwrap();
    assert!(!resumed.contains(&"hello".to_owned()), "{resumed:?}");
    assert!(resumed.contains(&"-model".to_owned()), "{resumed:?}");
    assert!(
        AutoArgs::parse(Provider::Agy, &args(&["-print", "x"]))
            .unwrap()
            .one_shot
    );
}
