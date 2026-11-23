use aswap::store;
use ring::digest::{SHA256, digest};
use serde_json::{Value, json};
use std::{fs, process::Command};

fn login(provider: &str, token: &str, id: &str) -> Value {
    if provider == "claude" {
        json!({"claudeAiOauth":{"accessToken":token,"refreshToken":token}})
    } else {
        json!({"tokens":{"access_token":token,"refresh_token":token,"account_id":id}})
    }
}

fn hex_digest(bytes: &[u8]) -> String {
    digest(&SHA256, bytes)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[test]
fn live_activation_leaves_the_only_pending_rotation_recoverable() {
    for provider in ["claude", "codex"] {
        for runtime_only in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path();
            let runtime = root.join("runtime");
            fs::create_dir(&runtime).unwrap();
            let store = root.join("data/aswap");
            let (default_dir, auth_file) = if provider == "claude" {
                (root.join("home/.claude"), ".credentials.json")
            } else {
                (root.join("home/.codex"), "auth.json")
            };
            let auth = default_dir.join(auth_file);
            let cli = |args: &[&str]| {
                let proc = Command::new(env!("CARGO_BIN_EXE_aswap"))
                    .env_clear()
                    .env("HOME", root.join("home"))
                    .env("XDG_DATA_HOME", root.join("data"))
                    .env("XDG_CONFIG_HOME", root.join("config"))
                    .env("XDG_RUNTIME_DIR", &runtime)
                    .args(args)
                    .output()
                    .unwrap();
                assert!(proc.status.success(), "{args:?}: {:?}", proc.stderr);
            };
            let save_default = |label: &str| {
                store::json_write(&auth, &login(provider, label, label)).unwrap();
                if provider == "claude" {
                    store::json_write(
                        &root.join("home/.claude.json"),
                        &json!({"oauthAccount":{"accountUuid":label}}),
                    )
                    .unwrap();
                }
            };
            for label in ["outgoing", "incoming"] {
                save_default(label);
                cli(&["add", provider, label]);
            }
            save_default("outgoing");
            let profile = store.join("profiles").join(provider).join("outgoing");
            let saved = profile.join(auth_file);
            let original = fs::read(&saved).unwrap();
            let pending = if runtime_only {
                let prefix = hex_digest(store.as_os_str().as_encoded_bytes());
                runtime.join("aswap").join(format!(
                    "{}-{provider}-outgoing.pending.json",
                    &prefix[..16]
                ))
            } else {
                profile.join(format!("{auth_file}.pending"))
            };
            let rotated = json!({
                "replaces":hex_digest(&original),
                "auth":login(provider, "synthetic-rotated", "outgoing")
            });
            store::json_write(&pending, &rotated).unwrap();
            cli(&["switch", provider, "incoming", "--activate-live"]);
            assert_eq!(fs::read(&saved).unwrap(), original);
            assert!(
                pending.is_file(),
                "{provider} lost sole pending copy (runtime_only={runtime_only})"
            );
            assert_eq!(store::json_read::<Value>(&pending).unwrap(), rotated);
            assert_eq!(
                store::json_read::<Value>(&auth).unwrap(),
                login(provider, "incoming", "incoming")
            );
        }
    }
}
