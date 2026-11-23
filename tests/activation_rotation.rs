//! `switch --activate-live` and a concurrent token rotation (agy.3 review,
//! t4hf's activation control). Synthetic logins only.
use ring::digest::{SHA256, digest};
use serde_json::{Value, json};
use std::{fs, path::Path, process::Command};

fn login(provider: &str, account: &str, value: &str) -> Value {
    let token = format!("synthetic-{value}");
    if provider == "codex" {
        json!({"tokens":{"account_id":account,"access_token":token,"refresh_token":token}})
    } else {
        json!({"claudeAiOauth":{"accessToken":token,"refreshToken":token}})
    }
}
/// A login with an expiry: the newer one may expire sooner.
fn expiring(provider: &str, account: &str, value: &str, expires_ms: u64) -> Value {
    let mut auth = login(provider, account, value);
    if provider == "codex" {
        auth["last_refresh"] = json!("2026-11-23T00:00:00Z");
        let claims = json!({"exp": expires_ms / 1000});
        use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
        auth["tokens"]["access_token"] = json!(format!(
            "synthetic.{}.synthetic",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
        ));
    } else {
        auth["claudeAiOauth"]["expiresAt"] = json!(expires_ms);
    }
    auth
}
fn write(path: &Path, data: &Value) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let draft = path.with_extension("review-draft");
    fs::write(&draft, serde_json::to_vec(data).unwrap()).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&draft, fs::Permissions::from_mode(0o600)).unwrap();
    fs::rename(&draft, path).unwrap();
}
fn hex(bytes: &[u8]) -> String {
    digest(&SHA256, bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

struct Case {
    _tmp: tempfile::TempDir,
    root: std::path::PathBuf,
    provider: &'static str,
    auth_file: &'static str,
}
impl Case {
    fn new(provider: &'static str) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_owned();
        fs::create_dir(root.join("runtime")).unwrap();
        let auth_file = if provider == "codex" {
            "auth.json"
        } else {
            ".credentials.json"
        };
        let case = Self {
            _tmp: tmp,
            root,
            provider,
            auth_file,
        };
        for label in ["outgoing", "incoming"] {
            case.default_login(label);
            case.ok(&["add", provider, label]);
        }
        case.default_login("outgoing");
        case
    }
    fn default(&self) -> std::path::PathBuf {
        self.root
            .join("home")
            .join(format!(".{}", self.provider))
            .join(self.auth_file)
    }
    fn default_login(&self, account: &str) {
        write(
            &self.default(),
            &login(self.provider, account, &format!("{account}-old")),
        );
        if self.provider == "claude" {
            write(
                &self.root.join("home/.claude.json"),
                &json!({"oauthAccount":{"accountUuid":account}}),
            );
        }
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_aswap"));
        command
            .args(args)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("HOME", self.root.join("home"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_RUNTIME_DIR", self.root.join("runtime"));
        command
    }
    fn ok(&self, args: &[&str]) {
        let out = self.command(args).output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    fn saved(&self, label: &str) -> std::path::PathBuf {
        self.root
            .join("data/aswap/profiles")
            .join(self.provider)
            .join(label)
            .join(self.auth_file)
    }
    fn read(path: &Path) -> Value {
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }
}

#[test]
fn activating_an_account_with_a_pending_rotation_installs_the_rotation() {
    for provider in ["codex", "claude"] {
        let case = Case::new(provider);
        let saved = case.saved("incoming");
        let old = fs::read(&saved).unwrap();
        let rotated = login(provider, "incoming", "incoming-rotated");
        let sidecar = saved.with_file_name(format!("{}.pending", case.auth_file));
        write(&sidecar, &json!({"replaces": hex(&old), "auth": rotated}));
        case.ok(&["switch", provider, "incoming", "--activate-live"]);
        assert_eq!(
            Case::read(&case.default()),
            rotated,
            "{provider}: never the revoked login"
        );
        assert!(!sidecar.exists());
    }
}

#[test]
fn an_unreadable_pending_rotation_is_kept_and_refuses_the_activation() {
    for provider in ["codex", "claude"] {
        let case = Case::new(provider);
        let saved = case.saved("incoming");
        let sidecar = saved.with_file_name(format!("{}.pending", case.auth_file));
        fs::write(&sidecar, b"{ not json").unwrap();
        let default_before = fs::read(case.default()).unwrap();
        let out = case
            .command(&["switch", provider, "incoming", "--activate-live"])
            .output()
            .unwrap();
        assert!(!out.status.success(), "{provider}: refused");
        assert!(sidecar.exists(), "{provider}: the pending copy is kept");
        assert_eq!(
            fs::read(case.default()).unwrap(),
            default_before,
            "{provider}: nothing activated"
        );
    }
}

/// A real rotation (the refresh installer, a mocked token response) while
/// a `switch --activate-live` waits for the store lock: the activation runs
/// after it and must keep the new login, whatever its expiry.
fn queued_activation_after_rotation(provider: &'static str, expires_in: u64) {
    use aswap::{
        coordination, refresh,
        store::{Account, Store},
    };
    let case = Case::new(provider);
    let store = Store::open(case.root.join("data/aswap")).unwrap();
    coordination::owner(&store).unwrap();
    let account = Account {
        provider: if provider == "codex" {
            aswap::provider::Provider::Codex
        } else {
            aswap::provider::Provider::Claude
        },
        label: "outgoing".into(),
    };
    // The live default and the profile hold the same login, which expires
    // far later than the rotated one will.
    let old = expiring(provider, "outgoing", "outgoing-old", 9_000_000_000_000);
    write(&case.default(), &old);
    write(&case.saved("outgoing"), &old);
    let guard = store.lock().unwrap();
    let mut child = case
        .command(&["switch", provider, "incoming", "--activate-live"])
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(
        child.try_wait().unwrap().is_none(),
        "activation waits for the store lock"
    );
    let at = aswap::now();
    assert!(
        refresh::refresh_with(&store, &account, at, true, |_, _| Ok(json!({
            "access_token": "synthetic-rotated-access",
            "refresh_token": "synthetic-rotated-refresh",
            "expires_in": expires_in,
        })))
        .unwrap()
    );
    let installed = fs::read(case.saved("outgoing")).unwrap();
    drop(guard);
    assert!(child.wait().unwrap().success());
    assert_eq!(
        fs::read(case.saved("outgoing")).unwrap(),
        installed,
        "{provider}: the new login stays"
    );
    assert_eq!(
        Case::read(&case.default()),
        login(provider, "incoming", "incoming-old")
    );
}

#[test]
fn an_activation_queued_behind_a_completed_rotation_keeps_the_new_login() {
    for provider in ["codex", "claude"] {
        queued_activation_after_rotation(provider, 3600);
    }
}

#[test]
fn a_rotation_with_a_shorter_expiry_is_still_the_newer_login() {
    for provider in ["codex", "claude"] {
        // The default login expires far later than the rotated one.
        let case_expiry = 60;
        queued_activation_after_rotation(provider, case_expiry);
    }
}
