use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};
struct Sandbox {
    temp: tempfile::TempDir,
}
impl Sandbox {
    fn new() -> Self {
        Self {
            temp: tempfile::tempdir().unwrap(),
        }
    }
    fn root(&self) -> &Path {
        self.temp.path()
    }
    fn invoke(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_aswap"))
            .args(args)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("HOME", self.root())
            .env("XDG_DATA_HOME", self.root().join("data"))
            .env("XDG_CONFIG_HOME", self.root().join("config"))
            .env("CLAUDE_CONFIG_DIR", self.root().join("claude"))
            .env("CODEX_HOME", self.root().join("codex"))
            .output()
            .unwrap()
    }
    fn ok(&self, args: &[&str]) -> Value {
        let out = self.invoke(args);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }
    fn seed(&self) {
        fs::create_dir_all(self.root().join("claude")).unwrap();
        fs::create_dir_all(self.root().join("codex")).unwrap();
        fs::write(self.root().join("claude/.credentials.json"),br#"{"claudeAiOauth":{"accessToken":"synthetic-private-token","refreshToken":"synthetic-private-refresh"}}"#).unwrap();
        fs::write(
            self.root().join("codex/auth.json"),
            br#"{"tokens":{"access_token":"synthetic-private-codex"}}"#,
        )
        .unwrap();
    }
}
#[test]
fn cli_lifecycle_json_and_waybar_schema() {
    let t = Sandbox::new();
    t.seed();
    assert!(t.invoke(&["--help"]).status.success());
    assert!(String::from_utf8_lossy(&t.invoke(&["--version"]).stdout).starts_with("aswap "));
    for p in ["claude", "codex"] {
        assert_eq!(
            t.ok(&["add", p, "work", "--json"])["added"]["label"],
            "work"
        );
    }
    let v = t.ok(&["list", "--json"]);
    assert_eq!(v["accounts"].as_array().unwrap().len(), 2);
    assert!(!v.to_string().contains("synthetic-private"));
    assert!(t.invoke(&["list"]).status.success());
    assert_eq!(
        t.ok(&["switch", "codex", "work", "--json"])["live_activated"],
        false
    );
    assert_eq!(
        t.ok(&["status", "codex", "--json"])["accounts"][0]["remaining_percent"],
        Value::Null
    );
    // Preload synthetic quota to ensure Waybar/usage do not touch a network or real CLI.
    for p in ["claude", "codex"] {
        let cached = json!({"usage":{"windows":[{"name":"five_hour","used_percent":40.0,"resets_at":null,"duration_minutes":300}]},"fetched_at":aswap::now(),"next_poll_at":aswap::now()+3600,"failures":0,"error":null});
        fs::write(
            t.root()
                .join(format!("data/aswap/profiles/{p}/work/usage-cache.json")),
            serde_json::to_vec(&cached).unwrap(),
        )
        .unwrap();
        assert_eq!(
            t.ok(&["usage", p, "--json"])["accounts"][0]["remaining_percent"],
            60.0
        );
    }
    let bar = t.ok(&["waybar", "--once"]);
    assert_eq!(bar["class"], "aswap");
    assert!(bar["text"].as_str().unwrap().contains("codex:work 60%"));
    assert!(!t.invoke(&["waybar", "--interval", "0"]).status.success());
    assert_eq!(
        t.ok(&["auto", "claude", "on", "--json"])["auto"]["enabled"],
        true
    );
    assert_eq!(
        t.ok(&["auto", "claude", "threshold", "85", "--json"])["auto"]["threshold"],
        85.0
    );
    assert_eq!(
        t.ok(&["auto", "claude", "once", "--json"])["selected"],
        Value::Null
    );
    assert_eq!(
        t.ok(&["auto", "claude", "off", "--json"])["auto"]["enabled"],
        false
    );
    for args in [
        &["auto", "claude", "threshold", "101", "--json"][..],
        &["add", "claude", "../escape", "--json"][..],
    ] {
        assert!(!t.invoke(args).status.success());
    }
    assert_eq!(
        t.ok(&["remove", "codex", "work", "--json"])["removed"],
        true
    );
    assert!(!t.invoke(&["switch", "codex", "work"]).status.success());
    assert!(
        !t.invoke(&["run", "claude", "--auto", "--", "--print", "synthetic"])
            .status
            .success()
    );
}
#[test]
fn errors_hide_bad_credentials_and_managed_files() {
    use std::os::unix::fs::symlink;
    let t = Sandbox::new();
    t.seed();
    fs::write(
        t.root().join("claude/.credentials.json"),
        b"invalid synthetic-private-token",
    )
    .unwrap();
    let out = t.invoke(&["add", "claude", "bad", "--json"]);
    assert!(!out.status.success());
    assert!(!String::from_utf8_lossy(&out.stderr).contains("synthetic-private-token"));
    fs::remove_file(t.root().join("codex/auth.json")).unwrap();
    let managed = t.root().join("managed");
    fs::write(
        &managed,
        br#"{"tokens":{"access_token":"synthetic-managed"}}"#,
    )
    .unwrap();
    symlink(&managed, t.root().join("codex/auth.json")).unwrap();
    assert!(!t.invoke(&["add", "codex", "managed"]).status.success());
    assert!(
        fs::read_to_string(managed)
            .unwrap()
            .contains("synthetic-managed")
    );
}
#[test]
fn auto_off_performs_no_usage_fetch() {
    let t = Sandbox::new();
    t.seed();
    t.ok(&["add", "claude", "one", "--json"]);
    t.ok(&["auto", "claude", "once", "--json"]);
    assert!(
        !t.root()
            .join("data/aswap/profiles/claude/one/usage-cache.json")
            .exists()
    );
}
