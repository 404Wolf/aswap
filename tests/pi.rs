//! pi as a provider, with synthetic pi and codex programs: profiles keep only
//! the ChatGPT login (moved out of pi's own auth.json), launches get a
//! private access-only login beside shared settings and sessions, and quota
//! comes from a usage-only Codex app-server.
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
};

/// Reports what pi would see, then exits 4.
const FAKE_PI: &str = r#"
import json, os, sys
home = os.environ['PI_CODING_AGENT_DIR']
auth = json.load(open(os.path.join(home, 'auth.json')))
print(json.dumps({'home': home, 'auth': auth, 'args': sys.argv[1:],
    'sessions': os.path.realpath(os.path.join(home, 'sessions')),
    'settings': open(os.path.join(home, 'settings.json')).read(),
    'openai_key': os.environ.get('OPENAI_API_KEY'),
    'auth_mode': os.stat(os.path.join(home, 'auth.json')).st_mode & 0o777}))
sys.exit(4)
"#;
/// A Codex app-server answering account/rateLimits/read, recording the auth
/// it was given.
const FAKE_CODEX: &str = r#"
import json, os, shutil, sys
assert sys.argv[-3:] == ['app-server', '--listen', 'stdio://'], sys.argv
shutil.copy(os.path.join(os.environ['CODEX_HOME'], 'auth.json'), os.environ['ASWAP_TEST_SEEN'])
for line in sys.stdin:
    req = json.loads(line)
    if req.get('method') == 'initialize':
        print(json.dumps({'id': req['id'], 'result': {}}), flush=True)
    elif req.get('method') == 'account/rateLimits/read':
        print(json.dumps({'id': req['id'], 'result': {'rateLimits': {
            'primary': {'usedPercent': 37, 'windowDurationMins': 300},
            'secondary': {'usedPercent': 12, 'windowDurationMins': 10080}}}}), flush=True)
"#;

fn jwt(claims: Value) -> String {
    format!(
        "e30.{}.synthetic",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
    )
}
fn login(account: &str) -> Value {
    json!({"type": "oauth",
        "access": jwt(json!({"https://api.openai.com/auth": {"chatgpt_account_id": account, "chatgpt_plan_type": "plus"}})),
        "refresh": format!("synthetic-refresh-{account}"),
        "expires": 4_000_000_000_000u64,
        "accountId": account})
}

struct Sandbox {
    temp: tempfile::TempDir,
}
impl Sandbox {
    fn new() -> Self {
        let t = Self {
            temp: tempfile::tempdir().unwrap(),
        };
        let python = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|p| p.join("python3"))
            .find(|p| p.is_file())
            .unwrap();
        fs::create_dir_all(t.bin()).unwrap();
        for (name, body) in [("pi", FAKE_PI), ("codex", FAKE_CODEX)] {
            let path = t.bin().join(name);
            fs::write(&path, format!("#!{}\n{body}", python.display())).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        fs::create_dir_all(t.pi()).unwrap();
        fs::write(t.pi().join("settings.json"), r#"{"theme":"synthetic"}"#).unwrap();
        t
    }
    fn root(&self) -> &Path {
        self.temp.path()
    }
    fn bin(&self) -> PathBuf {
        self.root().join("bin")
    }
    fn pi(&self) -> PathBuf {
        self.root().join("pi-agent")
    }
    fn write_auth(&self, auth: &Value) {
        let path = self.pi().join("auth.json");
        fs::write(&path, auth.to_string()).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    fn auth(&self) -> Value {
        serde_json::from_slice(&fs::read(self.pi().join("auth.json")).unwrap()).unwrap()
    }
    fn aswap(&self, args: &[&str]) -> Output {
        let path = std::env::join_paths(
            std::iter::once(self.bin())
                .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
        )
        .unwrap();
        Command::new(env!("CARGO_BIN_EXE_aswap"))
            .args(args)
            .env_clear()
            .env("PATH", path)
            .env("HOME", self.root())
            .env("XDG_DATA_HOME", self.root().join("data"))
            .env("XDG_CONFIG_HOME", self.root().join("config"))
            .env("PI_CODING_AGENT_DIR", self.pi())
            .env("ASWAP_TEST_SEEN", self.root().join("seen-auth.json"))
            .env("OPENAI_API_KEY", "synthetic-override")
            .output()
            .unwrap()
    }
    fn ok(&self, args: &[&str]) -> Value {
        let out = self.aswap(args);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }
    fn profile(&self, label: &str) -> PathBuf {
        self.root().join("data/aswap/profiles/pi").join(label)
    }
}

#[test]
fn add_moves_only_the_chatgpt_login_out_of_pi() {
    let t = Sandbox::new();
    let other = json!({"type": "api_key", "key": "synthetic-anthropic-key"});
    t.write_auth(&json!({"anthropic": other, "openai-codex": login("acct-one")}));
    assert_eq!(
        t.ok(&["add", "pi", "one", "--json"])["added"]["provider"],
        "pi"
    );
    let saved: Value =
        serde_json::from_slice(&fs::read(t.profile("one").join("auth.json")).unwrap()).unwrap();
    assert_eq!(saved, json!({"openai-codex": login("acct-one")}));
    // pi keeps its other logins; the ChatGPT one now lives only in aswap.
    assert_eq!(t.auth(), json!({"anthropic": other}));
    // Nothing left to add; a Sign in with ChatGPT login is a second account.
    assert!(!t.aswap(&["add", "pi", "again"]).status.success());
    let mut chatgpt = login("acct-two");
    chatgpt["clientId"] = json!("client-synthetic");
    t.write_auth(&json!({"anthropic": other, "openai": chatgpt}));
    t.ok(&["add", "pi", "two", "--json"]);
    let list = t.ok(&["list", "pi", "--json"]);
    let labels: Vec<_> = list["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| (a["label"].as_str().unwrap(), a["active"].as_bool().unwrap()))
        .collect();
    assert_eq!(labels, [("one", true), ("two", false)]);
    // pi's default login can't be swapped in place: it is pi's own file.
    assert!(
        !t.aswap(&["switch", "pi", "two", "--activate-live"])
            .status
            .success()
    );
    t.ok(&["switch", "pi", "two", "--json"]);
}

#[test]
fn a_pi_launch_has_a_private_access_only_login_and_shared_state() {
    let t = Sandbox::new();
    t.write_auth(&json!({"openai-codex": login("acct-one")}));
    t.ok(&["add", "pi", "one", "--json"]);
    fs::create_dir_all(t.pi().join("sessions/--synthetic--")).unwrap();
    let out = t.aswap(&["run", "pi", "--", "--model", "openai-codex/gpt-6.1-sol"]);
    assert_eq!(
        out.status.code(),
        Some(4),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let seen: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(seen["args"], json!(["--model", "openai-codex/gpt-6.1-sol"]));
    let home = PathBuf::from(seen["home"].as_str().unwrap());
    assert!(home.starts_with(t.root().join("data/aswap/runtimes/pi/one")));
    let entry = &seen["auth"]["openai-codex"];
    assert_eq!(entry["access"], login("acct-one")["access"]);
    assert_eq!(entry["refresh"], "aswap-owner-managed");
    assert_eq!(entry["expires"], 4_000_000_000_000u64 + 86_400_000);
    // Sessions and settings are the user's own.
    assert_eq!(
        PathBuf::from(seen["sessions"].as_str().unwrap()),
        fs::canonicalize(t.pi().join("sessions")).unwrap()
    );
    assert_eq!(seen["settings"], r#"{"theme":"synthetic"}"#);
    assert_eq!(seen["openai_key"], Value::Null);
    // The view's login file is private; pi's own auth.json is untouched.
    assert_eq!(seen["auth_mode"], 0o600);
    // The finished run's view is gone.
    assert!(!home.exists());
    assert_eq!(t.auth(), json!({}));
}

#[test]
fn pi_quota_comes_from_a_codex_app_server_with_access_only_auth() {
    let t = Sandbox::new();
    t.write_auth(&json!({"openai-codex": login("acct-one")}));
    t.ok(&["add", "pi", "one", "--json"]);
    let usage = t.ok(&["usage", "pi", "--json"]);
    let row = &usage["accounts"][0];
    assert_eq!(row["usage_status"], "ok");
    assert_eq!(row["remaining_percent"], 63.0);
    let seen: Value =
        serde_json::from_slice(&fs::read(t.root().join("seen-auth.json")).unwrap()).unwrap();
    assert_eq!(seen["tokens"]["access_token"], login("acct-one")["access"]);
    assert_eq!(seen["tokens"]["account_id"], "acct-one");
    assert_eq!(seen["tokens"]["refresh_token"], "");
    assert!(!seen.to_string().contains("synthetic-refresh"));
    // Sign in with ChatGPT has no quota to read: unknown, never a guess.
    let mut chatgpt = login("acct-two");
    chatgpt["clientId"] = json!("client-synthetic");
    t.write_auth(&json!({"openai": chatgpt}));
    t.ok(&["add", "pi", "two", "--json"]);
    let usage = t.ok(&["usage", "pi", "--account", "two", "--json"]);
    assert_eq!(
        usage["accounts"][0]["usage_error"]["class"],
        "usage-unsupported"
    );
    assert_eq!(usage["accounts"][0]["remaining_percent"], Value::Null);
    // Waybar names pi beside the other providers.
    let waybar = t.ok(&["waybar", "--once"]);
    assert!(waybar["text"].as_str().unwrap().contains("pi:one 63%"));
}

/// pi under `--auto`: on account one it records a ChatGPT usage limit in its
/// session transcript and stays open, as interactive pi does; on account two
/// it records the arguments it was resumed with and exits.
const FAKE_PI_AUTO: &str = r#"
import datetime, json, os, sys, time
home = os.environ['PI_CODING_AGENT_DIR']
assert os.isatty(0) and os.isatty(1)
args = sys.argv[1:]
access = json.load(open(os.path.join(home, 'auth.json')))['openai-codex']['access']
id = args[args.index('--session-id') + 1]
now = lambda: datetime.datetime.now(datetime.timezone.utc).isoformat(timespec='milliseconds').replace('+00:00', 'Z')
sessions = os.path.join(home, 'sessions', '--synthetic--')
os.makedirs(sessions, exist_ok=True)
path = os.path.join(sessions, '2026-11-18T00-00-00-000Z_' + id + '.jsonl')
if not os.path.exists(path):
    open(path, 'w').write(json.dumps({'type': 'session', 'id': id, 'timestamp': now()}) + '\n')
if access == os.environ['ASWAP_TEST_SECOND']:
    open(os.path.join(home, 'sessions', 'resumed-args.json'), 'w').write(json.dumps(args))
    sys.exit(0)
with open(path, 'a') as t:
    t.write(json.dumps({'type': 'message', 'id': 'a1', 'timestamp': now(), 'message': {
        'role': 'assistant', 'content': [], 'stopReason': 'error',
        'errorMessage': 'You have hit your ChatGPT usage limit (plus plan). Try again in ~42 min.'}}) + '\n')
print('You have hit your ChatGPT usage limit', flush=True)
time.sleep(60)
"#;

#[test]
fn auto_resumes_the_same_pi_session_on_another_account_at_a_limit() {
    let t = Sandbox::new();
    let python = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|p| p.join("python3"))
        .find(|p| p.is_file())
        .unwrap();
    fs::write(
        t.bin().join("pi"),
        format!("#!{}\n{FAKE_PI_AUTO}", python.display()),
    )
    .unwrap();
    for (label, account, used) in [("one", "acct-one", 20.0), ("two", "acct-two", 10.0)] {
        t.write_auth(&json!({"openai-codex": login(account)}));
        t.ok(&["add", "pi", label, "--json"]);
        let cache = json!({"usage": {"windows": [{"name": "five_hour", "used_percent": used,
            "resets_at": null, "duration_minutes": 300}]},
            "fetched_at": aswap::now(), "next_poll_at": aswap::now() + 3600});
        fs::write(t.profile(label).join("usage-cache.json"), cache.to_string()).unwrap();
    }
    let path = std::env::join_paths(
        std::iter::once(t.bin()).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_aswap"))
        .args([
            "run",
            "pi",
            "--account",
            "one",
            "--auto",
            "--",
            "--model",
            "openai-codex/gpt-6.1-sol",
        ])
        .env_clear()
        .env("PATH", path)
        .env("HOME", t.root())
        .env("XDG_DATA_HOME", t.root().join("data"))
        .env("XDG_CONFIG_HOME", t.root().join("config"))
        .env("PI_CODING_AGENT_DIR", t.pi())
        .env(
            "ASWAP_TEST_SECOND",
            login("acct-two")["access"].as_str().unwrap(),
        )
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(stderr.contains("aswap: using pi:one"), "{stderr}");
    assert!(stderr.contains("aswap: using pi:two"), "{stderr}");
    let args: Vec<String> =
        serde_json::from_slice(&fs::read(t.pi().join("sessions/resumed-args.json")).unwrap())
            .unwrap();
    // The same session, the same options, then the resume prompt.
    let sessions: Vec<_> = fs::read_dir(t.pi().join("sessions/--synthetic--"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(sessions.len(), 1, "{sessions:?}");
    let id = &sessions[0][sessions[0].len() - 42..sessions[0].len() - 6];
    // aswap's session extension, then the same session and options.
    assert_eq!(args[0], "-e");
    assert!(args[1].ends_with("/aswap/extensions/aswap-pi-session.ts"));
    let args = &args[2..];
    assert_eq!(
        args[..4],
        ["--session-id", id, "--model", "openai-codex/gpt-6.1-sol"]
    );
    assert_eq!(args[4], "--");
    assert!(args[5].contains("usage-limit account switch"));
}

/// Like FAKE_PI_AUTO, but on account one the user starts a new conversation
/// (pi's /new) before the limit: aswap's extension reports the switch, and
/// the limit is recorded in the new conversation's transcript.
const FAKE_PI_SWITCH: &str = r#"
import datetime, json, os, sys, time
home = os.environ['PI_CODING_AGENT_DIR']
args = sys.argv[1:]
extension = args[args.index('-e') + 1]
assert 'session_start' in open(extension).read()
report = os.environ['ASWAP_PI_SESSION_REPORT']
def reported(id):
    open(report + '.tmp', 'w').write(json.dumps({'sessionId': id}))
    os.rename(report + '.tmp', report)
access = json.load(open(os.path.join(home, 'auth.json')))['openai-codex']['access']
id = args[args.index('--session-id') + 1]
now = lambda: datetime.datetime.now(datetime.timezone.utc).isoformat(timespec='milliseconds').replace('+00:00', 'Z')
sessions = os.path.join(home, 'sessions', '--synthetic--')
os.makedirs(sessions, exist_ok=True)
transcript = lambda id: os.path.join(sessions, '2026-11-18T00-00-00-000Z_' + id + '.jsonl')
reported(id)
if access == os.environ['ASWAP_TEST_SECOND']:
    open(os.path.join(home, 'sessions', 'resumed-args.json'), 'w').write(json.dumps(args))
    sys.exit(0)
open(transcript(id), 'w').write(json.dumps({'type': 'session', 'id': id, 'timestamp': now()}) + '\n')
time.sleep(0.6)
new = 'aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee'
open(transcript(new), 'w').write(json.dumps({'type': 'session', 'id': new, 'timestamp': now()}) + '\n')
reported(new)
time.sleep(0.6)
with open(transcript(new), 'a') as t:
    t.write(json.dumps({'type': 'message', 'id': 'a1', 'timestamp': now(), 'message': {
        'role': 'assistant', 'content': [], 'stopReason': 'error',
        'errorMessage': 'You have hit your ChatGPT usage limit (plus plan).'}}) + '\n')
time.sleep(60)
"#;

#[test]
fn auto_follows_a_conversation_switch_inside_pi() {
    let t = Sandbox::new();
    let python = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|p| p.join("python3"))
        .find(|p| p.is_file())
        .unwrap();
    fs::write(
        t.bin().join("pi"),
        format!("#!{}\n{FAKE_PI_SWITCH}", python.display()),
    )
    .unwrap();
    for (label, account, used) in [("one", "acct-one", 20.0), ("two", "acct-two", 10.0)] {
        t.write_auth(&json!({"openai-codex": login(account)}));
        t.ok(&["add", "pi", label, "--json"]);
        let cache = json!({"usage": {"windows": [{"name": "five_hour", "used_percent": used,
            "resets_at": null, "duration_minutes": 300}]},
            "fetched_at": aswap::now(), "next_poll_at": aswap::now() + 3600});
        fs::write(t.profile(label).join("usage-cache.json"), cache.to_string()).unwrap();
    }
    let path = std::env::join_paths(
        std::iter::once(t.bin()).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_aswap"))
        .args(["run", "pi", "--account", "one", "--auto"])
        .env_clear()
        .env("PATH", path)
        .env("HOME", t.root())
        .env("XDG_DATA_HOME", t.root().join("data"))
        .env("XDG_CONFIG_HOME", t.root().join("config"))
        .env("PI_CODING_AGENT_DIR", t.pi())
        .env(
            "ASWAP_TEST_SECOND",
            login("acct-two")["access"].as_str().unwrap(),
        )
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(stderr.contains("aswap: using pi:two"), "{stderr}");
    let args: Vec<String> =
        serde_json::from_slice(&fs::read(t.pi().join("sessions/resumed-args.json")).unwrap())
            .unwrap();
    // The conversation pi switched to, not the one it was launched with.
    let at = args.iter().position(|a| a == "--session-id").unwrap();
    assert_eq!(args[at + 1], "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee");
}

#[test]
fn a_sign_in_with_chatgpt_refresh_is_form_encoded_with_its_own_client() {
    use aswap::{
        provider::{Encoding, Provider},
        refresh,
        store::{Account, Store},
    };
    let t = Sandbox::new();
    let mut chatgpt = login("acct-one");
    chatgpt["clientId"] = json!("client-synthetic");
    chatgpt["expires"] = json!(1_000_000u64);
    t.write_auth(&json!({"openai": chatgpt}));
    t.ok(&["add", "pi", "one", "--json"]);
    let store = Store::open(t.root().join("data/aswap")).unwrap();
    aswap::coordination::owner(&store).unwrap();
    let account = Account {
        provider: Provider::Pi,
        label: "one".into(),
    };
    let _lock = store.lock().unwrap();
    let rotated = refresh::refresh_request_with(&store, &account, 2000, false, |provider, request| {
        assert_eq!(provider, Provider::Pi);
        assert_eq!(request.url, "https://auth.openai.com/api/accounts/oauth/token");
        assert_eq!(request.encoding, Encoding::Form);
        assert_eq!(request.body["client_id"], "client-synthetic");
        assert_eq!(request.body["refresh_token"], "synthetic-refresh-acct-one");
        assert_eq!(request.body["resource"], "https://api.openai.com/v1");
        Ok(json!({"access_token": "synthetic-rotated", "refresh_token": "synthetic-refresh-rotated",
            "expires_in": 3600, "scope": "openid offline_access"}))
    })
    .unwrap();
    assert!(rotated);
    let saved: Value =
        serde_json::from_slice(&fs::read(t.profile("one").join("auth.json")).unwrap()).unwrap();
    assert_eq!(saved["openai"]["access"], "synthetic-rotated");
    assert_eq!(saved["openai"]["refresh"], "synthetic-refresh-rotated");
    assert_eq!(saved["openai"]["expires"], 5_600_000u64);
    assert_eq!(saved["openai"]["clientId"], "client-synthetic");
}
