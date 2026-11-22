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
        self.command(args).output().unwrap()
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_aswap"));
        command
            .args(args)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("HOME", self.root())
            .env("XDG_DATA_HOME", self.root().join("data"))
            .env("XDG_CONFIG_HOME", self.root().join("config"))
            .env("CLAUDE_CONFIG_DIR", self.root().join("claude"))
            .env("CODEX_HOME", self.root().join("codex"));
        command
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
fn reserve_round_trips_and_auto_launch_and_monitor_skip_the_label() {
    let t = Sandbox::new();
    t.seed();
    let at = aswap::now();
    for (label, used, reset) in [("work", 20, at + 3600), ("held", 1, at + 1200)] {
        fs::write(
            t.root().join("claude/.credentials.json"),
            json!({"claudeAiOauth":{"accessToken":format!("synthetic-{label}")}}).to_string(),
        )
        .unwrap();
        t.ok(&["add", "claude", label, "--json"]);
        fs::write(
            t.root().join(format!(
                "data/aswap/profiles/claude/{label}/usage-cache.json"
            )),
            json!({
                "usage":{"windows":[{"name":"seven_day","used_percent":used,"resets_at":reset,"duration_minutes":10080}]},
                "fetched_at":at,"next_poll_at":at+180
            })
            .to_string(),
        )
        .unwrap();
    }
    t.ok(&["add", "codex", "held", "--json"]);
    t.ok(&["auto", "claude", "prefer", "weekly-reset", "--json"]);
    t.ok(&["auto", "claude", "on", "--json"]);
    for _ in 0..2 {
        let v = t.ok(&["auto", "claude", "reserve", "held", "--json"]);
        assert_eq!(v["reserved"], true);
        assert_eq!(v["auto"]["reserved"], json!(["held"]));
    }
    for action in ["list", "status"] {
        let v = t.ok(&[action, "claude", "--json"]);
        let rows = v["accounts"].as_array().unwrap();
        assert_eq!(
            rows.iter().find(|r| r["label"] == "held").unwrap()["reserved"],
            true
        );
        assert_eq!(
            rows.iter().find(|r| r["label"] == "work").unwrap()["reserved"],
            false
        );
        let out = t.invoke(&[action, "claude"]);
        assert!(out.status.success());
        assert!(
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .any(|line| { line.contains("held") && line.contains("reserved") })
        );
    }
    assert_eq!(
        t.ok(&["list", "codex", "--json"])["accounts"][0]["reserved"],
        false
    );
    assert_eq!(
        t.ok(&["auto", "claude", "once", "--json"])["selected"],
        Value::Null
    );

    let fake = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|dir| dir.join("true"))
        .find(|path| path.is_file())
        .unwrap();
    let launch = |label: Option<&str>| {
        let mut cmd = t.command(&["run", "claude", "--auto"]);
        if let Some(label) = label {
            cmd.args(["--account", label]);
        }
        cmd.env("ASWAP_CLAUDE_BIN", &fake).output().unwrap()
    };
    t.ok(&["switch", "claude", "held", "--json"]);
    for (pin, chosen) in [(None, "work"), (Some("held"), "held")] {
        let out = launch(pin);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{stderr}");
        assert!(
            stderr.contains(&format!("using claude:{chosen} ")),
            "{stderr}"
        );
    }
    t.ok(&["auto", "claude", "reserve", "work", "--json"]);
    let refused = launch(None);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("no unreserved account"));
    t.ok(&["auto", "claude", "unreserve", "work", "--json"]);
    t.ok(&["switch", "claude", "work", "--json"]);
    let v = t.ok(&["auto", "claude", "unreserve", "held", "--json"]);
    assert_eq!(v["reserved"], false);
    assert_eq!(v["auto"]["reserved"], json!([]));
    assert_eq!(
        t.ok(&["auto", "claude", "once", "--json"])["selected"],
        "held"
    );
}
#[test]
fn gc_is_disabled_by_default_and_reads_declarative_config() {
    let sandbox = Sandbox::new();
    let report = sandbox.ok(&["gc", "--apply", "--json"]);
    assert_eq!(report["enabled"], false);
    assert!(!sandbox.root().join("data/aswap").exists());

    let config = sandbox.root().join("config/aswap");
    fs::create_dir_all(&config).unwrap();
    fs::write(config.join("config.toml"), "[retention]\nenabled = true\n").unwrap();
    let report = sandbox.ok(&["gc", "--json"]);
    assert_eq!(report["enabled"], true);
    assert_eq!(report["dry_run"], true);
    assert_eq!(report["totals"]["examined"], 0);
    assert!(!sandbox.root().join("data/aswap").exists());
    assert!(
        !sandbox
            .invoke(&["gc", "--dry-run", "--apply"])
            .status
            .success()
    );
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
    fs::write(t.root().join("data/aswap/profiles/codex/work/usage-cache.json"), serde_json::to_vec(&json!({"usage":{"windows":[]},"fetched_at":aswap::now(),"next_poll_at":aswap::now()+3600,"failures":0,"error":null})).unwrap()).unwrap();
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

#[test]
fn duplicate_warning_and_local_only_identity_output() {
    let t = Sandbox::new();
    t.seed();
    fs::write(t.root().join("claude/.claude.json"), serde_json::to_vec(&json!({"oauthAccount":{"accountUuid":"synthetic-account", "emailAddress":"alice@example.com"}})).unwrap()).unwrap();
    t.ok(&["add", "claude", "first", "--json"]);
    let out = t.invoke(&["add", "claude", "second"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("Warning: duplicate login of first"));
    let v = t.ok(&["list", "--json"]);
    assert_eq!(v["accounts"][0]["duplicates"], json!(["second"]));
    assert_eq!(v["accounts"][1]["duplicates"], json!(["first"]));
    assert!(!v.to_string().contains("alice@example.com"));
    assert!(!v.to_string().contains("synthetic-private"));
    fs::write(t.root().join("claude/.claude.json"), serde_json::to_vec(&json!({"oauthAccount":{"accountUuid":"synthetic-other-account", "emailAddress":"other@example.com"}})).unwrap()).unwrap();
    assert_eq!(
        t.ok(&["add", "claude", "third", "--json"])["duplicates"],
        json!([])
    );
    let out = t.invoke(&["list"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("alice@example.com"));
    assert!(text.contains("other@example.com"));
    assert!(text.contains("same login as second"));
    assert!(text.contains("never fetched"));
}

#[test]
fn rename_cli_and_resume_prompt_configuration() {
    let t = Sandbox::new();
    t.seed();
    t.ok(&["add", "claude", "old", "--json"]);
    let v = t.ok(&["rename", "claude", "old", "new", "--json"]);
    assert_eq!(v["renamed"]["label"], "new");
    assert_eq!(v["old_label"], "old");
    let list = t.ok(&["list", "--json"]);
    assert_eq!(list["accounts"][0]["label"], "new");
    assert_eq!(list["accounts"][0]["active"], true);
    t.ok(&["add", "claude", "collision", "--json"]);
    assert!(
        !t.invoke(&["rename", "claude", "new", "collision", "--json"])
            .status
            .success()
    );
    assert!(
        !t.invoke(&["rename", "claude", "new", "../escape", "--json"])
            .status
            .success()
    );
    assert_eq!(
        t.ok(&[
            "auto",
            "claude",
            "resume-prompt",
            "synthetic follow-up",
            "--json"
        ])["auto"]["resume_prompt"],
        "synthetic follow-up"
    );
    assert_eq!(
        t.ok(&["auto", "claude", "resume-prompt", "off", "--json"])["auto"]["resume_prompt"],
        Value::Null
    );
    assert!(
        !t.invoke(&["auto", "claude", "resume-prompt"])
            .status
            .success()
    );
    assert!(
        !t.invoke(&["auto", "claude", "resume-prompt", ""])
            .status
            .success()
    );
}
#[test]
fn remove_names_the_duplicate_and_json_contains_only_labels() {
    let t = Sandbox::new();
    t.seed();
    fs::write(t.root().join("claude/.claude.json"), serde_json::to_vec(&json!({"oauthAccount":{"accountUuid":"synthetic-account", "emailAddress":"alice@example.com"}})).unwrap()).unwrap();
    for label in ["first", "copy", "another"] {
        t.ok(&["add", "claude", label, "--json"]);
    }
    let removed = t.ok(&["remove", "claude", "copy", "--json"]);
    assert_eq!(removed["duplicates"], json!(["first", "another"]));
    assert!(!removed.to_string().contains("alice@example.com"));
    let out = t.invoke(&["remove", "claude", "another"]);
    assert!(out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout)
            .contains("Removed claude:another (duplicate login of first)")
    );
}

fn seed_codex(t: &Sandbox, label: &str, weekly: f64, reset: u64, credits: Value) {
    fs::write(
        t.root().join("codex/auth.json"),
        json!({"tokens":{"access_token":format!("synthetic-{label}")}}).to_string(),
    )
    .unwrap();
    t.ok(&["add", "codex", label, "--json"]);
    write_codex_cache(t, label, weekly, reset, credits);
}
fn write_codex_cache(t: &Sandbox, label: &str, weekly: f64, reset: u64, credits: Value) {
    let at = aswap::now();
    let count = credits.as_array().map_or(0, Vec::len);
    fs::write(
        t.root()
            .join(format!("data/aswap/profiles/codex/{label}/usage-cache.json")),
        json!({
            "usage":{
                "windows":[
                    {"name":"session","used_percent":10,"resets_at":at+3600,"duration_minutes":300,"codex_slot":0},
                    {"name":"weekly","used_percent":weekly,"resets_at":reset,"duration_minutes":10080,"codex_slot":1}
                ],
                "available_usage_resets":count,
                "reset_credits":credits,
                "reset_credits_listed":true
            },
            "fetched_at":at,"next_poll_at":at+180,"last_attempt":at
        })
        .to_string(),
    )
    .unwrap();
}

#[test]
fn codex_reset_mode_is_opt_in_shown_and_codex_only() {
    let t = Sandbox::new();
    t.seed();
    t.ok(&["add", "codex", "one", "--json"]);
    // Default off, including state written before the field existed.
    let state = t.root().join("data/aswap/state.json");
    let mut v: Value = serde_json::from_slice(&fs::read(&state).unwrap()).unwrap();
    v["auto"] = json!({"codex":{"enabled":true,"threshold":90.0,"last_switch":0}});
    fs::write(&state, v.to_string()).unwrap();
    let listed = t.ok(&["list", "--json"]);
    assert_eq!(listed["auto"]["codex"]["auto_reset"], "off");
    assert_eq!(listed["codex_usage_resets"]["halted"], Value::Null);
    let text = String::from_utf8_lossy(&t.invoke(&["list"]).stdout).into_owned();
    assert!(!text.contains("usage reset"), "{text}");
    for mode in ["dry-run", "on", "off", "dry-run"] {
        assert_eq!(
            t.ok(&["auto", "codex", "reset", mode, "--json"])["auto"]["auto_reset"],
            mode
        );
    }
    for action in ["list", "status"] {
        assert_eq!(
            t.ok(&[action, "--json"])["auto"]["codex"]["auto_reset"],
            "dry-run"
        );
        let out = t.command(&[action]).env("COLUMNS", "200").output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.contains("usage reset dry-run"), "{text}");
    }
    for bad in [
        &["auto", "claude", "reset", "on"][..],
        &["auto", "claude", "reset", "dry-run"],
        &["auto", "codex", "reset", "yes"],
        &["auto", "codex", "reset"],
    ] {
        assert!(!t.invoke(bad).status.success(), "{bad:?}");
    }
    t.ok(&["auto", "claude", "reset", "off", "--json"]);
}

#[test]
fn codex_monitor_prefers_soonest_reset_and_dry_run_only_logs_the_farthest() {
    let t = Sandbox::new();
    t.seed();
    let at = aswap::now();
    let credit = |id: &str| json!([{"id":id,"status":"available","reset_type":"codexRateLimits","title":"Full reset (Weekly + 5 hr)","description":"Ready to redeem"}]);
    seed_codex(&t, "late", 30.0, at + 6 * 86_400, credit("synthetic-late"));
    seed_codex(&t, "soon", 50.0, at + 2 * 86_400, credit("synthetic-soon"));
    t.ok(&["switch", "codex", "late", "--json"]);
    t.ok(&["auto", "codex", "prefer", "weekly-reset", "--json"]);
    t.ok(&["auto", "codex", "on", "--json"]);
    // Both have room: the monitor moves to the account whose week ends first.
    let v = t.ok(&["auto", "codex", "once", "--json"]);
    assert_eq!(v["selected"], "soon");
    assert!(v.get("usage_reset").is_none());
    // Both out of weekly usage, dry-run: report the farthest reset, spend nothing.
    write_codex_cache(&t, "late", 100.0, at + 6 * 86_400, credit("synthetic-late"));
    write_codex_cache(&t, "soon", 100.0, at + 2 * 86_400, credit("synthetic-soon"));
    t.ok(&["auto", "codex", "reset", "dry-run", "--json"]);
    // A codex binary that fails if anything starts it.
    let bin = t.root().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let marker = t.root().join("codex-started");
    let fake = bin.join("codex");
    fs::write(
        &fake,
        format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display()),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
    let out = t
        .command(&["auto", "codex", "once", "--json"])
        .env("ASWAP_CODEX_BIN", &fake)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["usage_reset"]["step"], "would-reset");
    assert_eq!(v["usage_reset"]["decision"]["label"], "late");
    assert!(v["usage_reset"]["decision"].get("credit_id").is_none());
    assert!(
        stderr.contains("dry-run: would reset codex:late (weekly resets in 6d"),
        "{stderr}"
    );
    assert!(!stderr.contains("synthetic-late"));
    assert!(!marker.exists(), "dry-run must not start codex");
    let history = &t.ok(&["list", "--json"])["codex_usage_resets"]["history"];
    assert_eq!(history[0]["outcome"], "dry-run");
    assert_eq!(history[0]["label"], "late");
    // The same decision is not logged again on the next tick.
    let out = t
        .command(&["auto", "codex", "once", "--json"])
        .env("ASWAP_CODEX_BIN", &fake)
        .output()
        .unwrap();
    assert!(!String::from_utf8_lossy(&out.stderr).contains("would reset"));
    assert!(!marker.exists());
    // Off reads nothing and reports nothing.
    t.ok(&["auto", "codex", "reset", "off", "--json"]);
    assert!(
        t.ok(&["auto", "codex", "once", "--json"])
            .get("usage_reset")
            .is_none()
    );
}

#[test]
fn switch_interval_is_global_defaults_to_four_seconds_and_shows_in_list_and_status() {
    let t = Sandbox::new();
    t.seed();
    t.ok(&["add", "claude", "work", "--json"]);
    // Cached synthetic quota: status never touches a network.
    fs::write(t.root().join("data/aswap/profiles/claude/work/usage-cache.json"), serde_json::to_vec(&json!({"usage":{"windows":[]},"fetched_at":aswap::now(),"next_poll_at":aswap::now()+3600,"failures":0,"error":null})).unwrap()).unwrap();
    // A state written before the setting existed loads with the default.
    let state = t.root().join("data/aswap/state.json");
    let mut old: Value = serde_json::from_slice(&fs::read(&state).unwrap()).unwrap();
    old.as_object_mut()
        .unwrap()
        .remove("switch_interval_seconds");
    fs::write(&state, serde_json::to_vec(&old).unwrap()).unwrap();
    assert_eq!(t.ok(&["list", "--json"])["switch_interval_seconds"], 4);
    assert_eq!(
        t.ok(&["auto", "switch-interval", "--json"])["switch_interval_seconds"],
        4
    );
    assert_eq!(
        t.ok(&["auto", "switch-interval", "9", "--json"])["switch_interval_seconds"],
        9
    );
    assert_eq!(t.ok(&["status", "--json"])["switch_interval_seconds"], 9);
    let text = t.invoke(&["list"]);
    assert!(
        String::from_utf8_lossy(&text.stdout)
            .contains("account switches: one at a time, at least 9s apart across all sessions"),
        "{}",
        String::from_utf8_lossy(&text.stdout)
    );
    t.ok(&["auto", "switch-interval", "0", "--json"]);
    let text = t.invoke(&["status"]);
    assert!(String::from_utf8_lossy(&text.stdout).contains("no machine-wide spacing"));
    // Per-provider verbs are unchanged and keep the global setting.
    t.ok(&["auto", "claude", "threshold", "80", "--json"]);
    assert_eq!(t.ok(&["list", "--json"])["switch_interval_seconds"], 0);
    assert!(
        !t.invoke(&["auto", "switch-interval", "3601"])
            .status
            .success()
    );
    assert!(
        !t.invoke(&["auto", "switch-interval", "-1"])
            .status
            .success()
    );
    assert!(!t.invoke(&["auto", "claude"]).status.success());
}

/// `list --json` is the stable, cache-only contract for other programs (the
/// Herdr Usage view polls it). Changing these keys or enums breaks them:
/// add fields, never rename or remove, and bump schema_version if you must.
#[test]
fn list_json_schema_is_pinned() {
    let t = Sandbox::new();
    t.seed();
    t.ok(&["add", "claude", "work", "--json"]);
    t.ok(&["add", "codex", "spare", "--json"]);
    let at = aswap::now();
    let cache = json!({"usage":{"windows":[{"name":"five_hour","used_percent":40.0,"resets_at":at+3600,"duration_minutes":300}]},"fetched_at":at-5,"next_poll_at":at+3600,"failures":0,"error":null});
    fs::write(
        t.root()
            .join("data/aswap/profiles/claude/work/usage-cache.json"),
        serde_json::to_vec(&cache).unwrap(),
    )
    .unwrap();
    let v = t.ok(&["list", "--json"]);
    let keys = |v: &Value| {
        let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
        k.sort();
        k
    };
    assert_eq!(v["schema_version"], 1);
    assert_eq!(
        keys(&v),
        [
            "accounts",
            "auto",
            "codex_usage_resets",
            "credential_mode",
            "last_sync",
            "schema_version",
            "switch_interval_seconds",
            "sync"
        ]
    );
    let rows = v["accounts"].as_array().unwrap();
    for row in rows {
        assert_eq!(
            keys(row),
            [
                "active",
                "age_seconds",
                "duplicates",
                "fetched_at",
                "http_status",
                "label",
                "last_attempt",
                "next_poll_at",
                "provider",
                "remaining_percent",
                "reserved",
                "retry_after_seconds",
                "usage",
                "usage_error",
                "usage_reason",
                "usage_status",
            ]
        );
        let status = row["usage_status"].as_str().unwrap();
        assert!(["ok", "backoff", "unknown"].contains(&status), "{status}");
    }
    let row = |label: &str| rows.iter().find(|r| r["label"] == label).unwrap();
    let work = row("work");
    assert_eq!(work["usage_status"], "ok");
    assert_eq!(work["usage_reason"], "fresh");
    assert_eq!(work["remaining_percent"], 60.0);
    assert!(work["age_seconds"].as_u64().unwrap() >= 5);
    assert_eq!(
        keys(&work["usage"]["windows"][0]),
        ["duration_minutes", "name", "resets_at", "used_percent"]
    );
    let spare = row("spare");
    assert_eq!(spare["usage_status"], "unknown");
    assert_eq!(spare["usage_reason"], "never-fetched");
    assert_eq!(spare["age_seconds"], Value::Null);
    assert!(!v.to_string().contains("synthetic-private"));
}
