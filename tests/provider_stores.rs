//! Verify the migration with private synthetic stores. Run the ignored
//! cross-version test with ASWAP_V021_BIN pointing at a build of tag v0.2.1
//! (8dc2a1829d22ea66a78c6164f4e5756e97ba7564).
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

const LEGACY_STATE: &str = include_str!("fixtures/v0.2.1/legacy-state.fixture.json");
const AGY: &str = r#"{"token":{"access_token":"synthetic-access","refresh_token":"synthetic-refresh","expiry":"2099-01-01T00:00:00Z"},"auth_method":"consumer","id_token_email":"fake@example.com"}"#;
const CLAUDE: &str =
    r#"{"claudeAiOauth":{"accessToken":"synthetic-claude","expiresAt":4070908800000}}"#;
const CODEX: &str = r#"{"tokens":{"access_token":"synthetic-codex","account_id":"synthetic-id"}}"#;

struct Sandbox(tempfile::TempDir);

impl Sandbox {
    fn new() -> Self {
        let t = Self(tempfile::tempdir().unwrap());
        t.write("data/aswap/state.json", LEGACY_STATE);
        t.write("data/aswap/profiles/claude/work/.credentials.json", CLAUDE);
        t.write("data/aswap/profiles/codex/work/auth.json", CODEX);
        t.write(
            "agy-data/aswap/state.json",
            &json!({
                "accounts":[{"provider":"agy","label":"work"}],
                "active":{"agy":"work"},
                "auto":{},
                "switch_interval_seconds": 23
            })
            .to_string(),
        );
        t.write(
            "agy-data/aswap/profiles/agy/work/antigravity-cli/antigravity-oauth-token",
            AGY,
        );
        let now = aswap::now();
        for (dir, used) in [
            ("data/aswap/profiles/claude", 12),
            ("data/aswap/profiles/codex", 26),
            ("agy-data/aswap/profiles/agy", 34),
        ] {
            t.write(&format!("{dir}/work/usage-cache.json"), &json!({
                "usage":{"windows":[{"name":"seven_day","used_percent":used,"resets_at":now+3600,"duration_minutes":10080}]},
                "fetched_at": now,
                "next_poll_at": now + 120
            }).to_string());
        }
        t
    }

    fn path(&self, path: &str) -> PathBuf {
        self.0.path().join(path)
    }

    fn write(&self, path: &str, content: &str) {
        let path = self.path(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn read(&self, path: &str) -> Value {
        serde_json::from_slice(&fs::read(self.path(path)).unwrap()).unwrap()
    }

    fn command(&self, bin: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(bin);
        command
            .args(args)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("HOME", self.0.path())
            .env("XDG_DATA_HOME", self.path("data"))
            .env("XDG_CONFIG_HOME", self.path("config"))
            .env("CODEX_HOME", self.path("codex"))
            .env("CLAUDE_CONFIG_DIR", self.path("claude"))
            .current_dir(self.0.path());
        command
    }

    fn native(&self, args: &[&str]) -> Command {
        let mut command = self.command(Path::new(env!("CARGO_BIN_EXE_aswap")), args);
        command
            .env("ASWAP_AGY_HOME", self.path("agy-data/aswap"))
            .env("ASWAP_AGY_PRIVATE_ROOT", self.path("private"));
        command
    }

    fn cli(&self, args: &[&str]) -> Value {
        checked(self.native(args).arg("--json").output().unwrap())
    }
}

fn checked(output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn providers(value: &Value) -> Vec<&str> {
    value["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["provider"].as_str().unwrap())
        .collect()
}

#[test]
fn joins_cached_rows_and_routes_writes_by_typed_provider() {
    let t = Sandbox::new();
    t.cli(&["auto", "agy", "reserve", "work"]);
    t.cli(&["auto", "codex", "threshold", "73"]);
    let combined = t.cli(&["list"]);
    assert_eq!(combined["schema_version"], 1);
    assert_eq!(combined["switch_interval_seconds"], 11);
    assert_eq!(providers(&combined), ["claude", "codex", "agy"]);
    assert_eq!(combined["accounts"][0]["remaining_percent"], 88.0);
    assert_eq!(combined["accounts"][1]["remaining_percent"], 74.0);
    assert!(combined["accounts"][2]["remaining_percent"].is_null());
    assert_eq!(
        combined["accounts"][2]["usage"]["windows"][0]["used_percent"],
        34.0
    );
    assert_eq!(combined["accounts"][2]["reserved"], true);
    assert_eq!(combined["auto"]["codex"]["threshold"], 73.0);
    assert_eq!(combined["auto"]["agy"]["reserved"], json!(["work"]));

    let agy = t.cli(&["status", "agy"]);
    assert_eq!(providers(&agy), ["agy"]);
    assert_eq!(agy["switch_interval_seconds"], 23);
    assert!(agy["auto"]["codex"].is_null());

    let legacy = t.read("data/aswap/state.json");
    assert_eq!(
        legacy["accounts"],
        serde_json::from_str::<Value>(LEGACY_STATE).unwrap()["accounts"]
    );
    assert!(legacy["auto"]["agy"].is_null());
    let separate = t.read("agy-data/aswap/state.json");
    assert!(separate["auto"]["codex"].is_null());
    assert_eq!(separate["active"], json!({"agy":"work"}));

    // A filtered command must work even when the unselected root is unreadable.
    t.write("agy-data/aswap/state.json", "{broken");
    assert_eq!(providers(&t.cli(&["list", "claude"])), ["claude"]);
    assert!(!t.native(&["list"]).output().unwrap().status.success());
}

#[test]
fn scoped_roots_refuse_aliases_and_wrong_provider_state() {
    let t = Sandbox::new();
    for path in ["data/aswap", "data/aswap/configured"] {
        let output = t
            .native(&["auto", "agy", "on"])
            .env("ASWAP_AGY_HOME", t.path(path))
            .output()
            .unwrap();
        assert!(!output.status.success());
    }
    let output = t
        .native(&["auto", "agy", "on"])
        .env("ASWAP_AGY_HOME", "relative")
        .output()
        .unwrap();
    assert!(!output.status.success());
    t.write("agy-data/aswap/state.json", LEGACY_STATE);
    let output = t.native(&["auto", "agy", "on"]).output().unwrap();
    assert!(!output.status.success());
    assert_eq!(
        fs::read_to_string(t.path("agy-data/aswap/state.json")).unwrap(),
        LEGACY_STATE
    );
}

#[test]
fn the_legacy_fence_survives_missing_and_alternate_config() {
    let t = Sandbox::new();
    let native = Path::new(env!("CARGO_BIN_EXE_aswap"));
    let root = t.path("data/aswap");
    let marker = root.join("legacy-compatibility");
    let preexisting = aswap::store::Store::open(root.clone()).unwrap();
    assert!(!preexisting.legacy_compatibility());

    let pending = "data/aswap/profiles/codex/work/auth.json.pending";
    t.write(pending, "synthetic pending sidecar");
    let refused = t.native(&["list", "agy"]).output().unwrap();
    assert!(!refused.status.success());
    assert!(!marker.exists(), "a failed admission persisted a marker");
    fs::remove_file(t.path(pending)).unwrap();
    assert_eq!(providers(&t.cli(&["list", "agy"])), ["agy"]);
    assert!(marker.is_file());
    assert!(preexisting.legacy_compatibility());
    assert!(
        !t.native(&["list", "agy"])
            .env("XDG_DATA_HOME", t.path("unrelated-data"))
            .env("ASWAP_AGY_HOME", &root)
            .output()
            .unwrap()
            .status
            .success()
    );

    let live = "codex/auth.json";
    t.write(live, "synthetic live sentinel");
    for config in [None, Some(t.path("alternate-config"))] {
        for (args, reason) in [
            (&["auto", "agy", "on"][..], "separate configured store"),
            (
                &["switch", "codex", "work", "--activate-live"][..],
                "disables live activation",
            ),
        ] {
            let mut cmd = t.command(native, args);
            match &config {
                Some(path) => {
                    cmd.env("XDG_CONFIG_HOME", path);
                }
                None => {
                    cmd.env_remove("XDG_CONFIG_HOME");
                }
            }
            let out = cmd.output().unwrap();
            assert!(!out.status.success());
            assert!(String::from_utf8_lossy(&out.stderr).contains(reason));
        }
    }
    assert_eq!(
        fs::read_to_string(root.join("state.json")).unwrap(),
        LEGACY_STATE
    );
    assert_eq!(
        fs::read_to_string(t.path(live)).unwrap(),
        "synthetic live sentinel"
    );
    assert_eq!(
        fs::read_to_string(root.join("profiles/codex/work/auth.json")).unwrap(),
        CODEX
    );

    fs::write(marker, "unknown marker format").unwrap();
    let refused = t
        .command(native, &["auto", "claude", "on"])
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("invalid legacy compatibility marker")
    );
}

#[test]
fn persistent_config_reuses_the_existing_private_view_and_child_environment() {
    let t = Sandbox::new();
    let agy_store = t.path("agy-data/aswap");
    let private_root = t.path(".local/state/agy-private");
    t.write(
        "managed.toml",
        &format!(
            "[stores.agy]\nroot = {:?}\nprogram = {:?}\n",
            agy_store.display().to_string(),
            t.path("fake-agy").display().to_string()
        ),
    );
    fs::create_dir_all(t.path("config/aswap")).unwrap();
    std::os::unix::fs::symlink(t.path("managed.toml"), t.path("config/aswap/config.toml")).unwrap();
    fs::set_permissions(t.path("agy-data"), fs::Permissions::from_mode(0o700)).unwrap();
    t.write(
        ".gemini/antigravity-cli/antigravity-oauth-token",
        "unmanaged sentinel",
    );
    t.write(
        ".local/state/agy-private/home/.gemini/antigravity-cli/settings.json",
        "{}",
    );
    t.write(
        ".local/state/agy-private/config/synthetic",
        "private config",
    );
    let shell = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|dir| dir.join("sh"))
        .find(|path| path.is_file())
        .unwrap();
    t.write(
        "fake-agy",
        &format!(
            r#"#!{}
set -eu
view=${{1#--gemini_dir=}}
test "$HOME" = "$TEST_PRIVATE/home"
test "$XDG_DATA_HOME" = "$TEST_AGY_DATA"
test "$XDG_CONFIG_HOME" = "$view/.config"
test "$XDG_STATE_HOME" = "$TEST_PRIVATE/state"
test "$XDG_RUNTIME_DIR" = "$TEST_PRIVATE/runtime"
test "$XDG_CACHE_HOME" = "$TEST_PRIVATE/cache"
test -z "${{DBUS_SESSION_BUS_ADDRESS+x}}"
test -f "$view/antigravity-cli/antigravity-oauth-token"
test "$(readlink "$view/antigravity-cli/brain")" = "$HOME/.gemini/antigravity-cli/brain"
test "$(cat "$XDG_CONFIG_HOME/synthetic")" = 'private config'
"#,
            shell.display()
        ),
    );
    fs::set_permissions(t.path("fake-agy"), fs::Permissions::from_mode(0o700)).unwrap();

    let native = Path::new(env!("CARGO_BIN_EXE_aswap"));
    let result = t
        .command(native, &["run", "agy"])
        .env("ASWAP_AGY_BIN", t.path("invalid-wrapper"))
        .env("DBUS_SESSION_BUS_ADDRESS", "synthetic")
        .env("TEST_PRIVATE", &private_root)
        .env("TEST_AGY_DATA", t.path("agy-data"))
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        fs::read_to_string(t.path(".gemini/antigravity-cli/antigravity-oauth-token")).unwrap(),
        "unmanaged sentinel"
    );
    assert!(!t.path(".gemini/antigravity-cli/brain").exists());
    assert_eq!(
        providers(&checked(
            t.command(native, &["list", "--json"]).output().unwrap()
        )),
        ["claude", "codex", "agy"]
    );
}

#[test]
#[ignore = "requires ASWAP_V021_BIN built from the exact v0.2.1 tag"]
fn v021_supervisor_and_writers_share_the_legacy_state() {
    let old = PathBuf::from(std::env::var_os("ASWAP_V021_BIN").expect("set ASWAP_V021_BIN"));
    let version = Command::new(&old).arg("--version").output().unwrap();
    assert_eq!(version.stdout, b"aswap 0.2.1\n");
    let t = Sandbox::new();
    let legacy = |args: &[&str]| checked(t.command(&old, args).arg("--json").output().unwrap());
    assert_eq!(providers(&legacy(&["list"])), ["claude", "codex"]);

    let shell = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|dir| dir.join("sh"))
        .find(|path| path.is_file())
        .unwrap();
    t.write(
        "fake-provider",
        &format!(
            "#!{}\n: > \"$TEST_STARTED\"\nwhile [ ! -f \"$TEST_FINISH\" ]; do sleep 0.05; done\n",
            shell.display()
        ),
    );
    fs::set_permissions(t.path("fake-provider"), fs::Permissions::from_mode(0o700)).unwrap();
    let mut running = t
        .command(&old, &["run", "codex", "--account", "work"])
        .env("ASWAP_CODEX_BIN", t.path("fake-provider"))
        .env("HC_TERMINAL_ID", "tsynthetic")
        .env("TEST_STARTED", t.path("started"))
        .env("TEST_FINISH", t.path("finish"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Release the child even when an assertion panics.
    struct Release(PathBuf);
    impl Drop for Release {
        fn drop(&mut self) {
            let _ = fs::write(&self.0, "");
        }
    }
    let _release = Release(t.path("finish"));
    let until = Instant::now() + Duration::from_secs(5);
    while !t.path("started").exists() {
        assert!(
            running.try_wait().unwrap().is_none(),
            "v0.2.1 supervisor exited"
        );
        assert!(Instant::now() < until, "fake provider did not start");
        std::thread::sleep(Duration::from_millis(20));
    }

    let writers = std::sync::Barrier::new(3);
    std::thread::scope(|scope| {
        let old_writer = || {
            writers.wait();
            for _ in 0..12 {
                legacy(&["auto", "claude", "threshold", "67"]);
                assert_eq!(providers(&legacy(&["list"])), ["claude", "codex"]);
            }
        };
        let new_writer = || {
            writers.wait();
            for _ in 0..12 {
                t.cli(&["auto", "codex", "threshold", "76"]);
                t.cli(&["auto", "agy", "threshold", "85"]);
            }
        };
        scope.spawn(old_writer);
        scope.spawn(new_writer);
        writers.wait();
        for _ in 0..12 {
            assert_eq!(providers(&t.cli(&["list"])), ["claude", "codex", "agy"]);
            let sessions = t.cli(&["sessions"]);
            assert_eq!(sessions["sessions"][0]["terminal"], "tsynthetic");
            assert_eq!(sessions["sessions"][0]["provider"], "codex");
        }
    });

    let legacy_result = legacy(&["list"]);
    assert_eq!(legacy_result["auto"]["claude"]["threshold"], 67.0);
    assert_eq!(legacy_result["auto"]["codex"]["threshold"], 76.0);
    assert_eq!(t.cli(&["list"])["auto"]["agy"]["threshold"], 85.0);
    t.write("finish", "");
    let result = running.wait_with_output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(t.cli(&["sessions"])["sessions"], json!([]));
    assert_eq!(
        t.read("data/aswap/profiles/codex/work/auth.json"),
        serde_json::from_str::<Value>(CODEX).unwrap()
    );
    assert_eq!(
        t.read("agy-data/aswap/profiles/agy/work/antigravity-cli/antigravity-oauth-token"),
        serde_json::from_str::<Value>(AGY).unwrap()
    );
}
