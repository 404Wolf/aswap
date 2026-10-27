use aswap::{
    provider::{self, Provider},
    shared::{self, View},
    store::{self, Store},
};
use serde_json::json;
use std::{fs, os::unix::fs::symlink, process::Command};
#[test]
fn synthetic_shared_layout_migration_and_managed_files() {
    let t = tempfile::tempdir().unwrap();
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "shared_child", "--nocapture"])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("HOME", t.path())
        .env("XDG_DATA_HOME", t.path().join("data"))
        .env("CLAUDE_CONFIG_DIR", t.path().join("claude"))
        .env("CODEX_HOME", t.path().join("codex"))
        .env("ASWAP_SHARED_CHILD", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
#[test]
fn shared_child() {
    if std::env::var("ASWAP_SHARED_CHILD").as_deref() != Ok("1") {
        return;
    }
    let store = Store::from_env().unwrap();
    for provider in [Provider::Claude, Provider::Codex] {
        let home = provider.current_home().unwrap();
        let auth = if provider == Provider::Claude {
            json!({"claudeAiOauth":{"accessToken":"synthetic-profile-token"}})
        } else {
            json!({"tokens":{"access_token":"synthetic-profile-token"}})
        };
        store::json_write(&home.join(provider.auth_file()), &auth).unwrap();
        if provider == Provider::Claude {
            store::json_write(&home.join(".claude.json"),&json!({"oauthAccount":{"accountUuid":"synthetic-profile"},"userID":"synthetic-private-id","primaryApiKey":"synthetic-private-api","projects":{"synthetic":{"hasTrustDialogAccepted":true}}})).unwrap();
        }
        let a = provider::add(&store, provider, "one").unwrap();
        let profile = store.profile(provider, "one").unwrap();
        let kind = if provider == Provider::Claude {
            "projects"
        } else {
            "sessions"
        };
        store::atomic(
            &home.join(kind).join("existing.jsonl"),
            b"synthetic shared original",
        )
        .unwrap();
        store::atomic(
            &profile.join(kind).join("existing.jsonl"),
            b"synthetic isolated collision",
        )
        .unwrap();
        store::atomic(
            &profile.join(kind).join("new.jsonl"),
            b"synthetic isolated new",
        )
        .unwrap();
        store::atomic(
            &profile.join("sessions").join("retained/history"),
            b"synthetic retained",
        )
        .unwrap();
        store::atomic(
            &store
                .sessions(provider, "one")
                .unwrap()
                .join("run-legacy")
                .join(kind)
                .join("auto.jsonl"),
            b"synthetic legacy auto",
        )
        .unwrap();
        store::atomic(&home.join("future-feature/state"), b"synthetic future").unwrap();
        let managed = home
            .parent()
            .unwrap()
            .join(format!("{}-managed-settings", provider));
        store::atomic(&managed, b"synthetic managed settings").unwrap();
        symlink(&managed, home.join("settings.json")).unwrap();
        let managed_dir = home
            .parent()
            .unwrap()
            .join(format!("{}-managed-skills", provider));
        store::atomic(&managed_dir.join("skill.md"), b"synthetic managed skill").unwrap();
        symlink(&managed_dir, home.join("skills")).unwrap();
        if provider == Provider::Claude {
            store::json_write(&home.join(".claude.json"),&json!({"oauthAccount":{"accountUuid":"synthetic-live"},"userID":"synthetic-live-id","projects":{"synthetic":{"hasTrustDialogAccepted":true}},"theme":"dark"})).unwrap();
        }
        let _lease = store.lease(&a).unwrap();
        let _second_lease = store.lease(&a).unwrap();
        let mut view = View::prepare(&store, &a).unwrap();
        assert_eq!(
            fs::read(home.join(kind).join("existing.jsonl")).unwrap(),
            b"synthetic shared original"
        );
        assert_eq!(
            fs::read(home.join(kind).join("new.jsonl")).unwrap(),
            b"synthetic isolated new"
        );
        assert!(home.join(kind).join("auto.jsonl").exists());
        assert!(home.join("sessions/retained/history").exists());
        assert!(!profile.join(kind).exists());
        assert!(view.path.join(kind).is_symlink());
        assert!(view.path.join("future-feature").is_symlink());
        assert!(!view.path.join(provider.auth_file()).is_symlink());
        assert!(!view.path.join("settings.json").is_symlink());
        assert!(!view.path.join("skills").is_symlink());
        fs::write(view.path.join("settings.json"), b"synthetic local edit").unwrap();
        fs::write(
            view.path.join("skills/skill.md"),
            b"synthetic local skill edit",
        )
        .unwrap();
        if provider == Provider::Claude {
            let mut config: serde_json::Value =
                store::json_read(&view.path.join(".claude.json")).unwrap();
            assert_eq!(config["oauthAccount"]["accountUuid"], "synthetic-profile");
            assert_eq!(config["userID"], "synthetic-private-id");
            assert_eq!(
                config["projects"]["synthetic"]["hasTrustDialogAccepted"],
                true
            );
            config["projects"]["synthetic"]["allowedTools"] = json!(["synthetic-tool"]);
            config["userID"] = json!("synthetic-profile-rotated-id");
            store::json_write(&view.path.join(".claude.json"), &config).unwrap();
            let mut concurrent: serde_json::Value =
                store::json_read(&home.join(".claude.json")).unwrap();
            concurrent["projects"]["other"] = json!({"synthetic":true});
            concurrent["theme"] = json!("light");
            store::json_write(&home.join(".claude.json"), &concurrent).unwrap();
        }
        store::atomic(
            &view.path.join(provider.auth_file()),
            &serde_json::to_vec(&if provider == Provider::Claude {
                json!({"claudeAiOauth":{"accessToken":"synthetic-rotated"}})
            } else {
                json!({"tokens":{"access_token":"synthetic-rotated"}})
            })
            .unwrap(),
        )
        .unwrap();
        let mut second_view = View::prepare(&store, &a).unwrap();
        let canonical = if provider == Provider::Claude {
            json!({"claudeAiOauth":{"accessToken":"synthetic-fresh-canonical","refreshToken":"synthetic-fresh-refresh"}})
        } else {
            json!({"tokens":{"access_token":"synthetic-fresh-canonical","refresh_token":"synthetic-fresh-refresh"}})
        };
        store::json_write(&profile.join(provider.auth_file()), &canonical).unwrap();
        view.sync(&store, &a).unwrap();
        second_view.sync(&store, &a).unwrap();
        assert_eq!(
            store::json_read::<serde_json::Value>(&profile.join(provider.auth_file())).unwrap(),
            canonical
        );
        for path in [&view.path, &second_view.path] {
            let current =
                String::from_utf8(store::read_private(&path.join(provider.auth_file())).unwrap())
                    .unwrap();
            assert!(current.contains("synthetic-fresh-canonical"));
            assert!(!current.contains("synthetic-fresh-refresh"));
        }
        assert_eq!(fs::read(&managed).unwrap(), b"synthetic managed settings");
        assert!(home.join("settings.json").is_symlink());
        assert_eq!(
            fs::read(managed_dir.join("skill.md")).unwrap(),
            b"synthetic managed skill"
        );
        assert!(
            String::from_utf8(fs::read(profile.join(provider.auth_file())).unwrap())
                .unwrap()
                .contains("synthetic-fresh-canonical")
        );
        if provider == Provider::Claude {
            let shared: serde_json::Value = store::json_read(&home.join(".claude.json")).unwrap();
            assert_eq!(shared["oauthAccount"]["accountUuid"], "synthetic-live");
            assert_eq!(shared["userID"], "synthetic-live-id");
            assert_eq!(shared["theme"], "light");
            assert_eq!(shared["projects"]["other"]["synthetic"], true);
            assert_eq!(
                shared["projects"]["synthetic"]["allowedTools"],
                json!(["synthetic-tool"])
            );
            let own: serde_json::Value = store::json_read(&profile.join(".claude.json")).unwrap();
            assert!(own.get("projects").is_none());
            assert_eq!(own["userID"], "synthetic-profile-rotated-id");
            // A managed global config is read, but never replaced or written through.
            let target = home.parent().unwrap().join("managed-global-config");
            fs::rename(home.join(".claude.json"), &target).unwrap();
            symlink(&target, home.join(".claude.json")).unwrap();
            let before = fs::read(&target).unwrap();
            let mut managed_view = View::prepare(&store, &a).unwrap();
            let mut config: serde_json::Value =
                store::json_read(&managed_view.path.join(".claude.json")).unwrap();
            config["theme"] = json!("synthetic local only");
            store::json_write(&managed_view.path.join(".claude.json"), &config).unwrap();
            managed_view.sync(&store, &a).unwrap();
            assert_eq!(fs::read(target).unwrap(), before);
            assert!(home.join(".claude.json").is_symlink());
        }
        // A live follower accepts owner updates without ever restoring its stale
        // runtime refresh token into the canonical profile.
        store::json_write(
            &store.root.join("coordination.json"),
            &aswap::coordination::Settings {
                owner: false,
                host: Some("synthetic-owner".into()),
                last_sync: aswap::now(),
            },
        )
        .unwrap();
        let fresh = if provider == Provider::Claude {
            json!({"claudeAiOauth":{"accessToken":"synthetic-owner-latest","expiresAt":9999999999999u64}})
        } else {
            json!({"tokens":{"access_token":"synthetic-owner-latest","refresh_token":""}})
        };
        store::json_write(&profile.join(provider.auth_file()), &fresh).unwrap();
        view.sync(&store, &a).unwrap();
        let canonical: serde_json::Value =
            store::json_read(&profile.join(provider.auth_file())).unwrap();
        assert_eq!(canonical, fresh);
        let running: serde_json::Value =
            store::json_read(&view.path.join(provider.auth_file())).unwrap();
        if provider == Provider::Claude {
            assert_eq!(
                running["claudeAiOauth"]["accessToken"],
                "synthetic-owner-latest"
            );
            assert_eq!(
                running["claudeAiOauth"]["refreshToken"],
                "aswap-owner-managed"
            );
        } else {
            assert_eq!(running["tokens"]["access_token"], "synthetic-owner-latest");
            assert_eq!(running["auth_mode"], "chatgptAuthTokens");
        }
        drop(_lease);
        drop(_second_lease);
        aswap::coordination::off(&store).unwrap();
        assert_eq!(
            store::json_read::<serde_json::Value>(&home.join(provider.auth_file())).unwrap(),
            auth
        );
    }
}
#[test]
fn three_way_merge_preserves_concurrent_values_and_honors_deletions() {
    let base = json!({"projects":{"one":{"trust":false,"same":1}},"deleted":true});
    let after = json!({"projects":{"one":{"trust":true,"same":1},"added":true}});
    let mut current =
        json!({"projects":{"one":{"trust":"concurrent","same":1},"other":true},"deleted":true});
    shared::merge_changes(&base, &after, &mut current);
    assert_eq!(
        current,
        json!({"projects":{"one":{"trust":"concurrent","same":1},"added":true,"other":true}})
    );
}
