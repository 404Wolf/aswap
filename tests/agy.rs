//! agy (Antigravity CLI) accounts: capture, an isolated launch, failover on
//! a quota notice with resume by conversation, and login write-back.
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
/// - stuck: account one prints a quota notice and does nothing else;
/// - redraw: account one prints the notice, then keeps working;
/// - refresh: account one writes a refreshed login (later expiry), exits.
const FAKE_AGY: &str = r##"import json, os, pathlib, sys, time
args=sys.argv[1:]
root=pathlib.Path(next(a.split('=',1)[1] for a in args if a.startswith('--gemini_dir=')))
args=[a for a in args if not a.startswith('--gemini_dir=')]
assert 'DBUS_SESSION_BUS_ADDRESS' not in os.environ and 'GEMINI_API_KEY' not in os.environ
assert os.environ['XDG_CONFIG_HOME']==str(root/'.config')
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
    login['token']['expiry']='2030-01-01T00:00:00Z'
    login['token']['access_token']='synthetic-a-refreshed'
    with token.open('w') as f: f.write(json.dumps(login))
    time.sleep(2.5)
    sys.exit(0)
print('Quota exhausted', flush=True)
if mode=='stuck':
    time.sleep(40); sys.exit(9)
for _ in range(20):
    time.sleep(1)
    with transcript.open('a') as f: f.write('{"type":"assistant"}\n')
sys.exit(0)
"##;

fn child(name: &str, mode: &str) -> (Vec<Value>, std::path::PathBuf, tempfile::TempDir) {
    let t = tempfile::tempdir().unwrap();
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
        .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/synthetic/bus")
        .env("GEMINI_API_KEY", "synthetic-billed")
        .env("ASWAP_AGY_BIN", &agy)
        .env("ASWAP_AGY_CHILD", name)
        .env("ASWAP_TEST_MODE", mode)
        .env("ASWAP_TEST_ID", ID)
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
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let home = t.path().join("home");
    (launches, home, t)
}
fn is_child(name: &str) -> bool {
    std::env::var("ASWAP_AGY_CHILD").as_deref() == Ok(name)
}
/// Two agy accounts, `one` (active) and `two`, captured from ~/.gemini.
fn two_accounts() -> Store {
    let store = Store::from_env().unwrap();
    let home = provider::home().unwrap();
    for name in ["one", "two"] {
        store::json_write(
            &token_path(&home),
            &login(&format!("{name}@example.com"), "2026-11-22T08:00:00Z"),
        )
        .unwrap();
        provider::add(&store, Provider::Agy, name).unwrap();
    }
    store
}

#[test]
fn capture_copies_the_login_and_refuses_billed_or_torn_ones() {
    child("capture_child", "none");
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
fn a_stuck_agy_at_its_quota_fails_over_and_resumes_the_conversation() {
    let (launches, _, _t) = child("run_child", "stuck");
    assert_eq!(launches.len(), 2, "{launches:?}");
    assert_eq!(launches[0]["account"], "one");
    assert_eq!(launches[1]["account"], "two");
    let resumed: Vec<String> = serde_json::from_value(launches[1]["args"].clone()).unwrap();
    assert_eq!(resumed[..2], ["--conversation".to_owned(), ID.to_owned()]);
    assert!(
        resumed[2].starts_with("--prompt-interactive="),
        "{resumed:?}"
    );
    assert_eq!(resumed.len(), 3, "{resumed:?}");
}

#[test]
fn a_redrawn_agy_notice_followed_by_work_never_switches() {
    let (launches, _, _t) = child("run_child", "redraw");
    assert_eq!(launches.len(), 1, "{launches:?}");
}

#[test]
fn a_login_agy_refreshed_in_its_view_is_written_back() {
    let (launches, home, _t) = child("run_child", "refresh");
    assert_eq!(launches.len(), 1, "{launches:?}");
    let data = home.parent().unwrap().join("data/aswap");
    let profile: Value = store::json_read(
        &data
            .join("profiles/agy/one")
            .join(Provider::Agy.auth_file()),
    )
    .unwrap();
    assert_eq!(profile["token"]["expiry"], "2030-01-01T00:00:00Z");
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
    let code = aswap::runner::run(&store, Provider::Agy, Some("one"), &[], true).unwrap();
    assert!(matches!(code, 0 | 9), "exit {code}");
}
