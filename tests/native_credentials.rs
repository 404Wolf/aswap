use aswap::{now, store};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use std::{
    fs::{self, File, FileTimes},
    path::{Path, PathBuf},
    process::{Command, Output},
    time::{Duration, SystemTime},
};

struct Fixture {
    root: tempfile::TempDir,
    provider: &'static str,
    auth_file: &'static str,
}

impl Fixture {
    fn new(provider: &'static str) -> Self {
        let fixture = Self {
            root: tempfile::tempdir().unwrap(),
            provider,
            auth_file: if provider == "claude" {
                ".credentials.json"
            } else {
                "auth.json"
            },
        };
        fs::create_dir(fixture.root.path().join("runtime")).unwrap();
        fixture
    }

    fn store(&self) -> PathBuf {
        self.root.path().join("data/aswap")
    }

    fn live(&self) -> PathBuf {
        self.root.path().join(self.provider).join(self.auth_file)
    }

    fn profile(&self, label: &str) -> PathBuf {
        self.store()
            .join("profiles")
            .join(self.provider)
            .join(label)
            .join(self.auth_file)
    }

    fn command_for(&self, binary: impl AsRef<Path>) -> Command {
        let root = self.root.path();
        let mut command = Command::new(binary.as_ref());
        command
            .env_clear()
            .env("HOME", root)
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_RUNTIME_DIR", root.join("runtime"))
            .env("CLAUDE_CONFIG_DIR", root.join("claude"))
            .env("CODEX_HOME", root.join("codex"));
        command
    }

    fn command(&self, verb: &str, label: &str, extra: &[&str]) -> Output {
        self.command_for(env!("CARGO_BIN_EXE_aswap"))
            .args([verb, self.provider, label])
            .args(extra)
            .output()
            .unwrap()
    }

    fn legacy_compatible(&self, binary: impl AsRef<Path>) -> Command {
        let mut command = self.command_for(binary);
        command.env("ASWAP_AGY_HOME", self.root.path().join("agy-private"));
        command
    }

    fn run(&self, verb: &str, label: &str, extra: &[&str]) {
        let output = self.command(verb, label, extra);
        assert!(
            output.status.success(),
            "{verb} {label}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn switch(&self, label: &str) {
        self.run("switch", label, &["--activate-live"]);
    }

    fn login(&self, account: &str, generation: &str, issued: Option<u64>) -> Value {
        let token = format!("synthetic-{account}-{generation}");
        if self.provider == "claude" {
            json!({"claudeAiOauth":{
                "accessToken":token,"refreshToken":token,"expiresAt":(now() + 3600) * 1000
            }})
        } else {
            let mut claims = json!({
                "https://api.openai.com/auth":{
                    "chatgpt_account_id":account, "chatgpt_account_user_id":format!("member-{account}")
                },
                "generation":generation, "exp":now() + 3600
            });
            if let Some(issued) = issued {
                claims["iat"] = json!(issued);
            }
            let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
            json!({"tokens":{
                "account_id":account,"access_token":format!("e30.{payload}.synthetic"),
                "refresh_token":token
            }})
        }
    }

    fn add(&self, label: &str, account: &str, auth: &Value) {
        store::json_write(&self.live(), auth).unwrap();
        if self.provider == "claude" {
            store::json_write(
                &self.live().with_file_name(".claude.json"),
                &json!({"oauthAccount":{"accountUuid":account}}),
            )
            .unwrap();
        }
        self.run("add", label, &[]);
    }

    fn activation(&self) -> PathBuf {
        fs::read_dir(self.store().join("live-activations"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
    }

    fn backdate_activation(&self, at: u64) {
        let path = self.activation();
        let mut record = read_login(&path);
        record["activated_at"] = json!(at);
        store::json_write(&path, &record).unwrap();
    }

    fn backup_of(&self, auth: &Value) -> PathBuf {
        fs::read_dir(self.store().join("live-backups").join(self.provider))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| read_login(&path.join(self.auth_file)) == *auth)
            .expect("outgoing login must survive in a backup")
    }

    fn pending(&self, label: &str, auth: &Value, runtime: bool) -> PathBuf {
        let path = if runtime {
            let prefix = digest(self.store().as_os_str().as_encoded_bytes());
            self.root.path().join("runtime/aswap").join(format!(
                "{}-{}-{label}.pending.json",
                &prefix[..16],
                self.provider
            ))
        } else {
            self.profile(label)
                .with_file_name(format!("{}.pending", self.auth_file))
        };
        store::json_write(
            &path,
            &json!({"replaces":digest(&fs::read(self.profile(label)).unwrap()), "auth":auth}),
        )
        .unwrap();
        path
    }
}

fn digest(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn read_login(path: &Path) -> Value {
    store::json_read(path).unwrap()
}

#[test]
fn recorded_codex_rotation_is_adopted_with_equal_mtimes_but_a_rollback_is_backed_up() {
    let f = Fixture::new("codex");
    let at = now();
    let old = f.login("one", "old", Some(at - 20));
    let mut rotation = f.login("one", "rotation", Some(at - 5));
    // An access token can rotate while the refresh token stays the same.
    rotation["tokens"]["refresh_token"] = old["tokens"]["refresh_token"].clone();
    f.add("one", "one", &old);
    f.add("alias", "one", &old);
    f.switch("one");
    f.backdate_activation(at - 10);
    store::json_write(&f.live(), &rotation).unwrap();
    for path in [f.live(), f.profile("one")] {
        File::open(path)
            .unwrap()
            .set_times(
                FileTimes::new().set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(at)),
            )
            .unwrap();
    }

    f.switch("one");
    for path in [f.live(), f.profile("one"), f.profile("alias")] {
        assert_eq!(read_login(&path), rotation);
    }

    fs::write(f.live(), serde_json::to_string(&old).unwrap()).unwrap();
    f.switch("alias");
    assert_eq!(read_login(&f.profile("one")), rotation);
    assert_eq!(read_login(&f.live()), rotation);
    f.backup_of(&old);
}

#[test]
fn unverifiable_live_rotations_are_backed_up_and_can_be_explicitly_restored() {
    for provider in ["claude", "codex"] {
        let f = Fixture::new(provider);
        let old = f.login("one", "old", None);
        let mut changed = f.login("one", "changed", None);
        if provider == "claude" {
            // Longer remaining lifetime isn't proof of later issuance.
            changed["claudeAiOauth"]["expiresAt"] = json!((now() + 7200) * 1000);
        } else {
            changed["tokens"]["refresh_token"] = old["tokens"]["refresh_token"].clone();
        }
        f.add("one", "one", &old);
        f.switch("one");
        store::json_write(&f.live(), &changed).unwrap();
        f.switch("one");
        assert_eq!(read_login(&f.profile("one")), old);
        assert_eq!(read_login(&f.live()), old);
        let backup = f.backup_of(&changed);
        f.run("restore", "one", &["--from", backup.to_str().unwrap()]);
        assert_eq!(read_login(&f.profile("one")), changed);
    }
}

#[test]
fn legacy_activation_record_is_readable_without_authorizing_adoption() {
    let f = Fixture::new("codex");
    let at = now();
    let old = f.login("one", "old", Some(at - 20));
    let changed = f.login("one", "changed", Some(at - 5));
    f.add("one", "one", &old);
    f.switch("one");
    let path = f.activation();
    let mut record = read_login(&path);
    record.as_object_mut().unwrap().remove("pair");
    record.as_object_mut().unwrap().remove("activated_at");
    store::json_write(&path, &record).unwrap();
    store::json_write(&f.live(), &changed).unwrap();
    f.switch("one");
    assert_eq!(read_login(&f.live()), old);
    f.backup_of(&changed);
}

#[test]
fn native_activation_leaves_alias_pending_base_intact_until_alias_recovery() {
    for provider in ["claude", "codex"] {
        for runtime in [false, true] {
            let f = Fixture::new(provider);
            let at = now();
            let old = f.login("one", "old", Some(at - 20));
            let pending = f.login("one", "pending", Some(at - 2));
            let competing = f.login("one", "native", Some(at - 5));
            let incoming = f.login("two", "old", Some(at - 20));
            f.add("one", "one", &old);
            f.add("alias", "one", &old);
            f.add("two", "two", &incoming);
            f.switch("one");
            f.backdate_activation(at - 10);
            let sidecar = f.pending("alias", &pending, runtime);
            let sidecar_before = fs::read(&sidecar).unwrap();
            let alias_before = fs::read(f.profile("alias")).unwrap();
            store::json_write(&f.live(), &competing).unwrap();

            f.switch("two");
            assert_eq!(read_login(&f.profile("one")), old);
            assert_eq!(fs::read(f.profile("alias")).unwrap(), alias_before);
            assert_eq!(fs::read(&sidecar).unwrap(), sidecar_before);
            assert_eq!(read_login(&f.live()), incoming);
            f.backup_of(&competing);

            // Switching to either label must recover the alias's sole copy.
            f.switch("one");
            assert!(!sidecar.exists());
            for path in [f.live(), f.profile("one"), f.profile("alias")] {
                assert_eq!(read_login(&path), pending);
            }
        }
    }
}

#[test]
fn unreadable_and_conflicting_alias_sidecars_stop_activation_without_writes() {
    let f = Fixture::new("claude");
    let old = f.login("one", "old", None);
    f.add("one", "one", &old);
    f.add("alias", "one", &old);
    f.switch("one");
    let pending = f.pending("one", &f.login("one", "pending", None), false);
    let other = f.pending("alias", &f.login("one", "conflict", None), true);
    for expected in [
        "conflicting pending logins",
        "a pending login can't be read",
    ] {
        let result = f.command("switch", "one", &["--activate-live"]);
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains(expected));
        assert!(pending.exists() && other.exists());
        for path in [f.live(), f.profile("one"), f.profile("alias")] {
            assert_eq!(read_login(&path), old);
        }
        fs::write(&other, b"unreadable as pending JSON").unwrap();
    }
}

#[test]
fn pending_rotation_finishes_alias_install_after_its_origin_was_written() {
    let f = Fixture::new("claude");
    let old = f.login("one", "old", None);
    let rotated = f.login("one", "rotated", None);
    f.add("one", "one", &old);
    f.add("alias", "one", &old);
    f.switch("one");
    let sidecar = f.pending("one", &rotated, false);
    let mut pending = read_login(&sidecar);
    pending["base_pair"] = read_login(&f.activation())["pair"].clone();
    store::json_write(&sidecar, &pending).unwrap();
    // Simulate a failure after the origin write but before alias propagation.
    store::json_write(&f.profile("one"), &rotated).unwrap();

    f.switch("alias");
    assert_eq!(read_login(&f.live()), rotated);
    assert_eq!(read_login(&f.profile("alias")), rotated);
    assert!(!sidecar.exists());
}

#[test]
fn legacy_compatibility_blocks_credential_writes_and_refuses_pending_by_presence() {
    let f = Fixture::new("claude");
    let old = f.login("one", "old", None);
    f.add("one", "one", &old);
    let legacy = |args: &[&str]| {
        f.legacy_compatible(env!("CARGO_BIN_EXE_aswap"))
            .args(args)
            .output()
            .unwrap()
    };

    assert!(legacy(&["switch", "claude", "one"]).status.success());
    for args in [
        vec!["switch", "claude", "one", "--activate-live"],
        vec!["restore", "claude", "one", "--from", "/synthetic-absent"],
    ] {
        let output = legacy(&args);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("legacy compatibility disables"));
    }
    assert_eq!(read_login(&f.live()), old);
    assert_eq!(read_login(&f.profile("one")), old);

    for runtime in [false, true] {
        let sidecar = f.pending("one", &f.login("one", "pending", None), runtime);
        // The filename alone must block preflight, even for an orphan.
        let orphan = sidecar.with_file_name(if runtime {
            sidecar
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .replace("one", "orphan")
        } else {
            sidecar.file_name().unwrap().to_string_lossy().into_owned()
        });
        if runtime {
            fs::rename(&sidecar, &orphan).unwrap();
        }
        fs::write(&orphan, b"synthetic secret: deliberately invalid JSON").unwrap();
        let refused = legacy(&["list", "claude"]);
        assert!(!refused.status.success());
        let error = String::from_utf8_lossy(&refused.stderr);
        assert!(error.contains("no pending logins"), "{error}");
        assert!(!error.contains("synthetic secret"));
        assert!(orphan.is_file());
        fs::remove_file(orphan).unwrap();
    }
    assert!(legacy(&["list", "claude"]).status.success());
}

#[test]
fn legacy_compatibility_skips_even_forced_or_expired_refresh() {
    let f = Fixture::new("claude");
    f.add("one", "one", &f.login("one", "old", None));
    let output = f
        .legacy_compatible(std::env::current_exe().unwrap())
        .env("ASWAP_SYNTHETIC_LEGACY_CHILD", "1")
        .args(["--exact", "legacy_compatibility_refresh_child"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn legacy_compatibility_refresh_child() {
    if std::env::var_os("ASWAP_SYNTHETIC_LEGACY_CHILD").is_none() {
        return;
    }
    let store = store::Store::from_env().unwrap();
    aswap::coordination::owner(&store).unwrap();
    let account = store
        .account(
            &store.load().unwrap(),
            aswap::provider::Provider::Claude,
            Some("one"),
        )
        .unwrap();
    let expired = now() + 7200;
    let unexpected = |_, _: &Value| panic!("legacy candidate must not make an OAuth request");
    assert!(!aswap::refresh::refresh_with(&store, &account, expired, true, unexpected).unwrap());
    assert!(!aswap::refresh::for_run_with(&store, &account, expired, unexpected).unwrap());
}

#[test]
#[ignore = "set ASWAP_V021_BIN to the exact deployed or tagged v0.2.1 executable"]
fn exact_old_activation_blocks_on_live_refresh_and_ignores_pending() {
    use fs2::FileExt;
    use std::{process::Stdio, time::Instant};

    let old_bin = PathBuf::from(std::env::var_os("ASWAP_V021_BIN").expect("set ASWAP_V021_BIN"));
    let f = Fixture::new("claude");
    let old_command = || f.command_for(&old_bin);
    let version = old_command().arg("--version").output().unwrap();
    assert_eq!(version.stdout, b"aswap 0.2.1\n");
    let original = f.login("one", "old", None);
    f.add("one", "one", &original);
    f.switch("one");
    let args = ["switch", "claude", "one", "--activate-live"];
    let lock_path = fs::read_dir(f.store())
        .unwrap()
        .flatten()
        .find(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .ends_with(".refresh.lock")
        })
        .unwrap()
        .path();
    let refresh = File::options().write(true).open(lock_path).unwrap();
    let global = File::options()
        .write(true)
        .open(f.store().join("store.lock"))
        .unwrap();
    refresh.lock_exclusive().unwrap();

    // The configured candidate fences its own activation before it can
    // wait on this lock. The exact old command goes on to take store.lock.
    let refused = f
        .legacy_compatible(env!("CARGO_BIN_EXE_aswap"))
        .args(args)
        .output()
        .unwrap();
    assert!(!refused.status.success());
    let mut child = old_command()
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    let blocked = loop {
        if global.try_lock_exclusive().is_err() {
            break child.try_wait().unwrap().is_none();
        }
        FileExt::unlock(&global).unwrap();
        if child.try_wait().unwrap().is_some() || Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    FileExt::unlock(&refresh).unwrap();
    assert!(child.wait().unwrap().success());
    assert!(blocked, "legacy activation did not wait holding store.lock");

    let rotated = f.login("one", "rotated", None);
    let sidecar = f.pending("one", &rotated, false);
    assert!(old_command().args(args).status().unwrap().success());
    assert_eq!(read_login(&f.live()), original);
    assert!(sidecar.is_file());
    // Outside mixed mode, the candidate can recover the same sidecar.
    f.switch("one");
    assert_eq!(read_login(&f.live()), rotated);
    assert!(!sidecar.exists());
}
