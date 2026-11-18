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
    'openai_key': os.environ.get('OPENAI_API_KEY')}))
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
    assert_eq!(
        fs::metadata(home.join("auth.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
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
