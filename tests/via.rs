//! `run claude --pick` and `run claude --via pi` with synthetic claude and pi
//! programs: piped stdio stays untouched, and the account follows quota.
use aswap::{
    provider::Provider,
    store::{self, Store},
    usage::{Cached, Usage, Window},
};
use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

/// Reads stdin line by line and answers each with one JSON line naming the
/// account token it ran with, as `claude -p --output-format stream-json`
/// would answer on a pipe.
const FAKE_CLAUDE: &str = r#"
import json, os, sys
home = os.environ['CLAUDE_CONFIG_DIR']
token = json.load(open(os.path.join(home, '.credentials.json')))['claudeAiOauth']['accessToken']
print('synthetic diagnostic', file=sys.stderr, flush=True)
for line in sys.stdin:
    print(json.dumps({'line': line.rstrip('\n'), 'token': token, 'args': sys.argv[1:],
                      'tty': os.isatty(0) or os.isatty(1)}), flush=True)
sys.exit(3)
"#;
/// Two turns, each one `claude -p` call through the provider path, as
/// pi-claude-code-provider makes them.
const FAKE_PI: &str = r#"
import json, os, subprocess, sys
claude = os.environ['PI_CLAUDE_CODE_PROVIDER_PATH']
turns = []
for turn in ['first', 'second']:
    r = subprocess.run([claude, '-p', '--output-format', 'stream-json'], input=turn + '\n',
                       capture_output=True, text=True)
    turns.append({'code': r.returncode, 'stdout': r.stdout})
print(json.dumps({'args': sys.argv[1:], 'provider_path': claude,
                  'config': os.environ.get('CLAUDE_CONFIG_DIR'), 'turns': turns}))
"#;

struct Sandbox {
    temp: tempfile::TempDir,
}
impl Sandbox {
    fn new() -> Self {
        let sandbox = Self {
            temp: tempfile::tempdir().unwrap(),
        };
        let python = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|p| p.join("python3"))
            .find(|p| p.is_file())
            .unwrap();
        fs::create_dir_all(sandbox.bin()).unwrap();
        for (name, body) in [("claude", FAKE_CLAUDE), ("pi", FAKE_PI)] {
            let path = sandbox.bin().join(name);
            fs::write(&path, format!("#!{}\n{body}", python.display())).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        fs::create_dir_all(sandbox.root().join("claude")).unwrap();
        for (label, used) in [("one", 95.0), ("two", 10.0)] {
            fs::write(
                sandbox.root().join("claude/.credentials.json"),
                json!({"claudeAiOauth": {"accessToken": format!("synthetic-{label}")}}).to_string(),
            )
            .unwrap();
            let out = sandbox.aswap(&["add", "claude", label], None);
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            sandbox.used(label, used);
        }
        sandbox
    }
    fn root(&self) -> &Path {
        self.temp.path()
    }
    fn bin(&self) -> PathBuf {
        self.root().join("bin")
    }
    fn store(&self) -> Store {
        Store::open(self.root().join("data/aswap")).unwrap()
    }
    /// A fresh cached reading, so no launch asks the network.
    fn used(&self, label: &str, used: f64) {
        let cached = Cached {
            usage: Usage {
                windows: vec![Window {
                    name: "five_hour".into(),
                    used_percent: used,
                    resets_at: None,
                    duration_minutes: Some(300),
                }],
                ..Default::default()
            },
            fetched_at: aswap::now(),
            next_poll_at: aswap::now() + 3600,
            ..Default::default()
        };
        let path = self
            .store()
            .profile(Provider::Claude, label)
            .unwrap()
            .join("usage-cache.json");
        store::json_write(&path, &cached).unwrap();
    }
    fn aswap(&self, args: &[&str], stdin: Option<&str>) -> Output {
        let path = std::env::join_paths(
            std::iter::once(self.bin())
                .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
        )
        .unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_aswap"))
            .args(args)
            .env_clear()
            .env("PATH", path)
            .env("HOME", self.root())
            .env("XDG_DATA_HOME", self.root().join("data"))
            .env("XDG_CONFIG_HOME", self.root().join("config"))
            .env("CLAUDE_CONFIG_DIR", self.root().join("claude"))
            .env("ASWAP_CLAUDE_BIN", self.bin().join("claude"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.unwrap_or("").as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }
}
fn lines(stdout: &str) -> Vec<Value> {
    assert!(
        !stdout.contains('\r'),
        "a PTY rewrote line endings: {stdout:?}"
    );
    stdout
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn pick_chooses_by_quota_and_keeps_pipes_plain() {
    let t = Sandbox::new();
    let out = t.aswap(
        &[
            "run",
            "claude",
            "--pick",
            "--",
            "-p",
            "--output-format",
            "stream-json",
        ],
        Some("hello\nworld\n"),
    );
    // The provider's own exit status, its stdout untouched, its stderr apart.
    assert_eq!(out.status.code(), Some(3));
    let stdout = String::from_utf8(out.stdout).unwrap();
    let answers = lines(&stdout);
    assert_eq!(answers.len(), 2, "input must not be echoed: {stdout:?}");
    for (answer, line) in answers.iter().zip(["hello", "world"]) {
        assert_eq!(answer["line"], line);
        // "one" is the default but above the 90% threshold; "two" is not.
        assert_eq!(answer["token"], "synthetic-two");
        assert_eq!(answer["tty"], false);
        assert_eq!(
            answer["args"],
            json!(["-p", "--output-format", "stream-json"])
        );
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("synthetic diagnostic"));
    assert!(stderr.contains("aswap: using claude:two"));
    // Picking never changes the default account.
    assert_eq!(t.store().load().unwrap().active[&Provider::Claude], "one");
}

#[test]
fn pick_refuses_an_account_or_auto() {
    let t = Sandbox::new();
    for args in [
        &["run", "claude", "--pick", "--account", "one"][..],
        &["run", "claude", "--pick", "--auto"][..],
    ] {
        assert!(!t.aswap(args, None).status.success());
    }
}

#[test]
fn via_pi_picks_an_account_for_every_claude_call() {
    let t = Sandbox::new();
    let run = |t: &Sandbox| {
        let out = t.aswap(
            &["run", "claude", "--via", "pi", "--", "--model", "x"],
            None,
        );
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice::<Value>(&out.stdout).unwrap()
    };
    let report = run(&t);
    assert_eq!(report["args"], json!(["--model", "x"]));
    let shim = PathBuf::from(report["provider_path"].as_str().unwrap());
    assert_eq!(shim, t.root().join("data/aswap/bin/claude-pick"));
    assert_eq!(
        fs::metadata(&shim).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let tokens = |report: &Value| -> Vec<String> {
        report["turns"]
            .as_array()
            .unwrap()
            .iter()
            .map(|turn| {
                assert_eq!(turn["code"], 3);
                let answers = lines(turn["stdout"].as_str().unwrap());
                assert_eq!(answers.len(), 1);
                answers[0]["token"].as_str().unwrap().to_owned()
            })
            .collect()
    };
    assert_eq!(tokens(&report), ["synthetic-two", "synthetic-two"]);
    // Quota moves: the next calls follow it, with no restart of pi needed.
    t.used("one", 5.0);
    t.used("two", 99.0);
    assert_eq!(tokens(&run(&t)), ["synthetic-one", "synthetic-one"]);
}

#[test]
fn via_pi_with_an_account_keeps_every_call_on_it() {
    let t = Sandbox::new();
    let out = t.aswap(&["run", "claude", "--via", "pi", "--account", "one"], None);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    // pi runs on one's launch view, and its provider runs the real claude.
    let view = report["config"].as_str().unwrap();
    assert!(
        view.starts_with(
            t.root()
                .join("data/aswap/runtimes/claude/one")
                .to_str()
                .unwrap()
        )
    );
    assert_eq!(
        report["provider_path"],
        t.bin().join("claude").to_str().unwrap()
    );
    for turn in report["turns"].as_array().unwrap() {
        assert_eq!(
            lines(turn["stdout"].as_str().unwrap())[0]["token"],
            "synthetic-one"
        );
    }
}

#[test]
fn via_pi_drives_claude_only() {
    let t = Sandbox::new();
    assert!(
        !t.aswap(&["run", "codex", "--via", "pi"], None)
            .status
            .success()
    );
    assert!(
        !t.aswap(&["run", "claude", "--via", "pi", "--auto"], None)
            .status
            .success()
    );
}

#[test]
fn pick_leaves_a_default_whose_quota_is_unknown() {
    let t = Sandbox::new();
    // The default's login expired: its reading is an error, its quota unknown.
    let path = t
        .store()
        .profile(Provider::Claude, "one")
        .unwrap()
        .join("usage-cache.json");
    let mut cached: Cached = store::json_read(&path).unwrap();
    cached.error = Some("http-401".into());
    store::json_write(&path, &cached).unwrap();
    let out = t.aswap(&["run", "claude", "--pick", "--", "-p"], Some("hi\n"));
    let answers = lines(&String::from_utf8(out.stdout).unwrap());
    assert_eq!(answers[0]["token"], "synthetic-two");
}
