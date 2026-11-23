//! `switch --activate-live` and the provider CLI's own rotations of the live
//! login (agy.7 review, t2kg's ruling). Synthetic logins only.
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use std::{fs, path::Path, path::PathBuf, process::Command};

/// A login naming its account with authority (#46): Codex's workspace
/// membership in the access token, Claude's account in its config.
fn login(provider: &str, account: &str, value: &str) -> Value {
    let token = format!("synthetic-{value}");
    if provider == "codex" {
        let claims = json!({"https://api.openai.com/auth": {
            "chatgpt_account_id": format!("workspace-{account}"),
            "chatgpt_account_user_id": format!("member-{account}"),
        }, "synthetic": value});
        let access = format!(
            "synthetic.{}.synthetic",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
        );
        json!({"tokens":{"account_id":format!("workspace-{account}"),"access_token":access,"refresh_token":token}})
    } else {
        json!({"claudeAiOauth":{"accessToken":token,"refreshToken":token}})
    }
}
fn write(path: &Path, data: &Value) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let draft = path.with_extension("test-draft");
    fs::write(&draft, serde_json::to_vec(data).unwrap()).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&draft, fs::Permissions::from_mode(0o600)).unwrap();
    fs::rename(&draft, path).unwrap();
}
fn read(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

struct Case {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    provider: &'static str,
    auth_file: &'static str,
    /// A second live slot (CODEX_HOME / CLAUDE_CONFIG_DIR), when set.
    slot: Option<PathBuf>,
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
            slot: None,
        };
        for label in ["a", "b"] {
            case.live(label, &format!("{label}-r1"));
            case.run(&["add", provider, label]);
        }
        case
    }
    fn slot_dir(&self) -> PathBuf {
        self.slot
            .clone()
            .unwrap_or_else(|| self.root.join("home").join(format!(".{}", self.provider)))
    }
    /// Put `account`'s login `value` live, as its CLI would.
    fn live(&self, account: &str, value: &str) {
        write(
            &self.slot_dir().join(self.auth_file),
            &login(self.provider, account, value),
        );
        if self.provider == "claude" {
            let config = match &self.slot {
                Some(dir) => dir.join(".claude.json"),
                None => self.root.join("home/.claude.json"),
            };
            write(
                &config,
                &json!({"oauthAccount":{"accountUuid":format!("uuid-{account}")}}),
            );
        }
    }
    fn live_login(&self) -> Value {
        read(&self.slot_dir().join(self.auth_file))
    }
    fn saved(&self, label: &str) -> Value {
        read(&self.profile(label).join(self.auth_file))
    }
    fn profile(&self, label: &str) -> PathBuf {
        self.root
            .join("data/aswap/profiles")
            .join(self.provider)
            .join(label)
    }
    fn backups(&self) -> Vec<PathBuf> {
        let dir = self
            .root
            .join("data/aswap/live-backups")
            .join(self.provider);
        let mut all: Vec<PathBuf> = fs::read_dir(dir)
            .map(|d| d.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        all.sort();
        all
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
        if let Some(dir) = &self.slot {
            let var = if self.provider == "codex" {
                "CODEX_HOME"
            } else {
                "CLAUDE_CONFIG_DIR"
            };
            command.env(var, dir);
        }
        command
    }
    /// Run aswap; its stderr.
    fn run(&self, args: &[&str]) -> String {
        let out = self.command(args).output().unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(out.status.success(), "{args:?}: {stderr}");
        stderr
    }
    fn fails(&self, args: &[&str]) -> String {
        let out = self.command(args).output().unwrap();
        assert!(!out.status.success(), "{args:?} succeeded");
        String::from_utf8_lossy(&out.stderr).into_owned()
    }
}

#[test]
fn the_clis_own_rotation_of_the_live_login_is_kept() {
    for provider in ["codex", "claude"] {
        let case = Case::new(provider);
        case.run(&["switch", provider, "a", "--activate-live"]);
        // The CLI rotates a's live login.
        case.live("a", "a-r2");
        let said = case.run(&["switch", provider, "b", "--activate-live"]);
        assert_eq!(case.saved("a"), login(provider, "a", "a-r2"), "{provider}");
        assert!(!said.contains("restore"), "{provider}: {said}");
        // Switching back puts the rotation live, never the revoked r1.
        case.run(&["switch", provider, "a", "--activate-live"]);
        assert_eq!(
            case.live_login(),
            login(provider, "a", "a-r2"),
            "{provider}"
        );
    }
}

#[test]
fn a_live_login_unchanged_since_aswap_put_it_there_is_never_saved() {
    for provider in ["codex", "claude"] {
        let case = Case::new(provider);
        case.run(&["switch", provider, "a", "--activate-live"]);
        // aswap's own refresh rotated a's profile login since; the live slot
        // still holds what aswap put there, an older login.
        write(
            &case.profile("a").join(case.auth_file),
            &login(provider, "a", "a-r3"),
        );
        case.run(&["switch", provider, "b", "--activate-live"]);
        assert_eq!(case.saved("a"), login(provider, "a", "a-r3"), "{provider}");
    }
}

#[test]
fn another_accounts_live_login_is_backed_up_said_and_restorable() {
    for provider in ["codex", "claude"] {
        let case = Case::new(provider);
        let before = (case.saved("a"), case.saved("b"));
        case.live("stranger", "stranger-r1");
        let said = case.run(&["switch", provider, "a", "--activate-live"]);
        assert_eq!((case.saved("a"), case.saved("b")), before, "{provider}");
        let backup = case.backups().pop().unwrap();
        assert!(
            said.contains(&format!(
                "aswap restore {provider} <label> --from {}",
                backup.display()
            )),
            "{provider}: {said}"
        );
        // Restoring it into a different account is refused.
        let backup = backup.to_str().unwrap();
        let refused = case.fails(&["restore", provider, "a", "--from", backup]);
        assert!(
            refused.contains("isn't a login of that account"),
            "{refused}"
        );
        assert_eq!(case.saved("a"), before.0, "{provider}");

        // A same-account backup restores.
        case.live("a", "a-r9");
        let _ = case.run(&["switch", provider, "b", "--activate-live"]);
        let backup = case
            .backups()
            .into_iter()
            .find(|b| read(&b.join(case.auth_file)) == login(provider, "a", "a-r9"));
        let backup = backup.unwrap();
        write(
            &case.profile("a").join(case.auth_file),
            &login(provider, "a", "a-r1"),
        );
        case.run(&["restore", provider, "a", "--from", backup.to_str().unwrap()]);
        assert_eq!(case.saved("a"), login(provider, "a", "a-r9"), "{provider}");
    }
}

#[test]
fn live_backups_are_pruned() {
    let case = Case::new("codex");
    for n in 0..24 {
        let label = if n % 2 == 0 { "a" } else { "b" };
        case.run(&["switch", "codex", label, "--activate-live"]);
    }
    assert!(case.backups().len() <= 20, "{}", case.backups().len());
}

#[test]
fn a_record_of_another_live_slot_never_saves_a_revoked_login() {
    // agy.8 review H1: the record belongs to one live slot.
    for provider in ["codex", "claude"] {
        let mut case = Case::new(provider);
        let other = case.root.join("slot-2");
        fs::create_dir_all(&other).unwrap();
        // Slot 2 puts a's r1 live.
        case.slot = Some(other.clone());
        case.run(&["switch", provider, "a", "--activate-live"]);
        // aswap's own refresh rotates a to r2 (r1 is revoked).
        write(
            &case.profile("a").join(case.auth_file),
            &login(provider, "a", "a-r2"),
        );
        // The default slot puts r2 live.
        case.slot = None;
        case.run(&["switch", provider, "a", "--activate-live"]);
        // Slot 2, still holding the revoked r1, switches away.
        case.slot = Some(other);
        case.run(&["switch", provider, "b", "--activate-live"]);
        assert_eq!(case.saved("a"), login(provider, "a", "a-r2"), "{provider}");
    }
}

#[test]
fn a_same_account_login_not_saved_is_reported() {
    // agy.8 review L1.
    for provider in ["codex", "claude"] {
        let case = Case::new(provider);
        // No activation record: a login made outside aswap.
        case.live("a", "a-elsewhere");
        let said = case.run(&["switch", provider, "b", "--activate-live"]);
        let backup = case.backups().pop().unwrap();
        assert!(said.contains("isn't provably newer"), "{provider}: {said}");
        assert!(
            said.contains(&backup.display().to_string()),
            "{provider}: {said}"
        );
    }
}

#[test]
fn restore_refuses_a_changed_or_pending_login_unless_forced_and_backs_it_up() {
    // agy.8 review M2.
    for provider in ["codex", "claude"] {
        let case = Case::new(provider);
        case.live("a", "a-elsewhere");
        case.run(&["switch", provider, "b", "--activate-live"]);
        let backup = case.backups().pop().unwrap();
        let from = backup.to_str().unwrap();
        // The profile changed since the backup: it may be newer.
        write(
            &case.profile("a").join(case.auth_file),
            &login(provider, "a", "a-newer"),
        );
        let refused = case.fails(&["restore", provider, "a", "--from", from]);
        assert!(refused.contains("changed since the backup"), "{refused}");
        assert_eq!(case.saved("a"), login(provider, "a", "a-newer"));
        // A pending rotation: refused too.
        let pending = case
            .profile("a")
            .join(format!("{}.pending", case.auth_file));
        write(
            &pending,
            &json!({"replaces": "synthetic", "auth": login(provider, "a", "a-pending")}),
        );
        let refused = case.fails(&["restore", provider, "a", "--from", from]);
        assert!(refused.contains("pending"), "{refused}");
        // Forced: both are backed up first, then the backup is installed.
        let before = case.backups();
        case.run(&["restore", provider, "a", "--from", from, "--force"]);
        assert_eq!(case.saved("a"), login(provider, "a", "a-elsewhere"));
        let made: Vec<_> = case
            .backups()
            .into_iter()
            .filter(|b| !before.contains(b))
            .collect();
        assert_eq!(made.len(), 1, "{provider}");
        assert_eq!(
            read(&made[0].join(case.auth_file)),
            login(provider, "a", "a-newer")
        );
        assert!(made[0].join("pending-0.json").exists(), "{provider}");
    }
}

#[test]
fn restore_of_an_unchanged_profile_needs_no_force() {
    for provider in ["codex", "claude"] {
        let case = Case::new(provider);
        case.live("a", "a-elsewhere");
        case.run(&["switch", provider, "b", "--activate-live"]);
        let backup = case.backups().pop().unwrap();
        case.run(&["restore", provider, "a", "--from", backup.to_str().unwrap()]);
        assert_eq!(case.saved("a"), login(provider, "a", "a-elsewhere"));
    }
}

#[test]
fn a_cli_rotation_of_a_later_alias_is_saved_to_it_and_its_aliases() {
    // agy.8 review M1: the alias that was put live, then propagated.
    for provider in ["codex", "claude"] {
        let case = Case::new(provider);
        case.live("a", "a-r1");
        case.run(&["add", provider, "a2"]);
        case.run(&["switch", provider, "a2", "--activate-live"]);
        case.live("a", "a-r2");
        case.run(&["switch", provider, "b", "--activate-live"]);
        assert_eq!(case.saved("a2"), login(provider, "a", "a-r2"), "{provider}");
        assert_eq!(case.saved("a"), login(provider, "a", "a-r2"), "{provider}");
    }
}
