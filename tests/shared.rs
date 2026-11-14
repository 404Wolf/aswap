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
            let mut config: serde_json::Value =
                store::json_read(&home.join(".claude.json")).unwrap();
            config["fullscreenBootPending"] = json!({"123": {"startedAt": 1}});
            config["fullscreenBootStrikes"] = json!({"count": 1});
            config["fullscreenAutoDisabled"] = json!({"strikes": 2});
            store::json_write(&home.join(".claude.json"), &config).unwrap();
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
            assert!(config.get("fullscreenBootPending").is_none());
            assert!(config.get("fullscreenBootStrikes").is_none());
            assert!(config.get("fullscreenAutoDisabled").is_none());
            config["projects"]["synthetic"]["allowedTools"] = json!(["synthetic-tool"]);
            config["userID"] = json!("synthetic-profile-rotated-id");
            config["fullscreenBootPending"] = json!({"456": {"startedAt": 2}});
            config["fullscreenBootStrikes"] = json!({"count": 3});
            config["fullscreenAutoDisabled"] = json!({"strikes": 3});
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
            assert_eq!(
                shared["fullscreenBootPending"],
                json!({"123": {"startedAt": 1}})
            );
            assert_eq!(shared["fullscreenBootStrikes"], json!({"count": 1}));
            assert_eq!(shared["fullscreenAutoDisabled"], json!({"strikes": 2}));
            // Restarting in the same view also discards the previous process's canary.
            view.select(&store, &a).unwrap();
            let restarted: serde_json::Value =
                store::json_read(&view.path.join(".claude.json")).unwrap();
            assert!(restarted.get("fullscreenBootPending").is_none());
            assert!(restarted.get("fullscreenBootStrikes").is_none());
            assert!(restarted.get("fullscreenAutoDisabled").is_none());
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
                ..Default::default()
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
#[test]
fn nested_launch_links_the_real_home_instead_of_copying_the_parent_view() {
    let t = tempfile::tempdir().unwrap();
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "nested_child", "--nocapture"])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("HOME", t.path())
        .env("XDG_DATA_HOME", t.path().join("data"))
        .env("CODEX_HOME", t.path().join("codex"))
        .env("ASWAP_NESTED_CHILD", "1")
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
fn nested_child() {
    if std::env::var("ASWAP_NESTED_CHILD").as_deref() != Ok("1") {
        return;
    }
    let store = Store::from_env().unwrap();
    let provider = Provider::Codex;
    let home = provider.current_home().unwrap();
    store::json_write(
        &home.join(provider.auth_file()),
        &json!({"tokens":{"access_token":"synthetic-profile-token"}}),
    )
    .unwrap();
    let account = provider::add(&store, provider, "one").unwrap();
    store::atomic(&home.join("sessions").join("old.jsonl"), b"synthetic").unwrap();
    let outer = View::prepare(&store, &account).unwrap();
    assert!(
        fs::symlink_metadata(outer.path.join("sessions"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    // A provider started inside that run inherits CODEX_HOME = its view.
    // SAFETY: this child process runs exactly one single-threaded test.
    unsafe { std::env::set_var("CODEX_HOME", &outer.path) };
    let inner = View::prepare(&store, &account).unwrap();
    assert_eq!(inner.home, home);
    let sessions = inner.path.join("sessions");
    assert!(
        fs::symlink_metadata(&sessions)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read_link(&sessions).unwrap(), home.join("sessions"));
    // A third level resolves the same way.
    unsafe { std::env::set_var("CODEX_HOME", &inner.path) };
    let third = View::prepare(&store, &account).unwrap();
    assert_eq!(third.home, home);
    // A view copied by an earlier release has no links left: fall back to
    // the provider's default home rather than copying it again.
    let copied = store.root.join("runtimes/codex/one/run-copied");
    store::atomic(&copied.join("sessions/old.jsonl"), b"synthetic").unwrap();
    unsafe { std::env::set_var("CODEX_HOME", &copied) };
    let fallback = View::prepare(&store, &account).unwrap();
    assert_eq!(fallback.home, provider::home().unwrap().join(".codex"));
}
#[test]
fn a_rotated_profile_token_reaches_a_running_view_without_the_30s_tick() {
    let t = tempfile::tempdir().unwrap();
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "auth_change_child", "--nocapture"])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("HOME", t.path())
        .env("XDG_DATA_HOME", t.path().join("data"))
        .env("CLAUDE_CONFIG_DIR", t.path().join("claude"))
        .env("CODEX_HOME", t.path().join("codex"))
        .env("ASWAP_AUTH_CHANGE_CHILD", "1")
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
fn auth_change_child() {
    if std::env::var("ASWAP_AUTH_CHANGE_CHILD").as_deref() != Ok("1") {
        return;
    }
    let store = Store::from_env().unwrap();
    let home = Provider::Claude.current_home().unwrap();
    let auth = |token: &str| json!({"claudeAiOauth":{"accessToken":token,"refreshToken":"synthetic-refresh","expiresAt":9999999999999u64}});
    store::json_write(
        &home.join(".credentials.json"),
        &auth("synthetic-old-access"),
    )
    .unwrap();
    store::json_write(
        &home.join(".claude.json"),
        &json!({"oauthAccount":{"accountUuid":"synthetic-profile"}}),
    )
    .unwrap();
    let a = provider::add(&store, Provider::Claude, "one").unwrap();
    let mut view = View::prepare(&store, &a).unwrap();
    let view_auth = view.path.join(".credentials.json");
    let launched = || -> serde_json::Value { store::json_read(&view_auth).unwrap() };
    assert!(!view.auth_changed(&store, &a));
    assert_eq!(
        launched()["claudeAiOauth"]["accessToken"],
        "synthetic-old-access"
    );
    // The owner rotates (or a follower installs a pushed packet): the old
    // access token is revoked at once, so the view must not wait 30s.
    let profile = store.profile(Provider::Claude, "one").unwrap();
    store::json_write(
        &profile.join(".credentials.json"),
        &auth("synthetic-new-access"),
    )
    .unwrap();
    assert!(view.auth_changed(&store, &a));
    view.sync(&store, &a).unwrap();
    let now = launched();
    assert_eq!(now["claudeAiOauth"]["accessToken"], "synthetic-new-access");
    // The view still carries only the owner-managed sentinel, never the
    // refresh token, and the change is consumed.
    assert_eq!(now["claudeAiOauth"]["refreshToken"], "aswap-owner-managed");
    assert!(!view.auth_changed(&store, &a));
}
#[test]
fn a_refresh_hands_the_new_token_to_every_live_view_at_once() {
    let t = tempfile::tempdir().unwrap();
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "hand_out_child", "--nocapture"])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("HOME", t.path())
        .env("XDG_DATA_HOME", t.path().join("data"))
        .env("CLAUDE_CONFIG_DIR", t.path().join("claude"))
        .env("CODEX_HOME", t.path().join("codex"))
        .env("ASWAP_HAND_OUT_CHILD", "1")
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
fn hand_out_child() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    if std::env::var("ASWAP_HAND_OUT_CHILD").as_deref() != Ok("1") {
        return;
    }
    const VIEWS: usize = 40;
    let store = Store::from_env().unwrap();
    let home = Provider::Claude.current_home().unwrap();
    let auth = |token: &str| json!({"claudeAiOauth":{"accessToken":token,"refreshToken":"synthetic-refresh","expiresAt":9999999999999u64}});
    let mut accounts = Vec::new();
    for (label, token) in [
        ("one", "synthetic-old-access"),
        ("two", "synthetic-two-access"),
    ] {
        store::json_write(&home.join(".credentials.json"), &auth(token)).unwrap();
        store::json_write(
            &home.join(".claude.json"),
            &json!({"oauthAccount":{"accountUuid":format!("synthetic-{label}")}}),
        )
        .unwrap();
        accounts.push(provider::add(&store, Provider::Claude, label).unwrap());
    }
    let (one, two) = (&accounts[0], &accounts[1]);
    aswap::coordination::owner(&store).unwrap();
    // Every run registers its live lease and names its view, as `aswap run` does.
    let mut runs = (0..VIEWS)
        .map(|_| {
            let lease = store.lease(one).unwrap();
            let view = View::prepare(&store, one).unwrap();
            lease.view(&view.path).unwrap();
            (lease, view)
        })
        .collect::<Vec<_>>();
    // A native MCP login in one view survives the hand-out untouched.
    let native = runs[0].1.path.join(".credentials.json");
    let mut with_mcp: serde_json::Value = store::json_read(&native).unwrap();
    with_mcp["mcpOAuth"] = json!({"synthetic-server":{"accessToken":"synthetic-mcp"}});
    store::json_write(&native, &with_mcp).unwrap();
    // A run that has just switched to another account still has a lease for
    // the old one: it must keep the new account's token.
    let moved_lease = store.lease(one).unwrap();
    let mut moved = View::prepare(&store, one).unwrap();
    moved_lease.view(&moved.path).unwrap();
    moved.select(&store, two).unwrap();
    let paths: Vec<_> = runs
        .iter()
        .map(|(_, view)| view.path.join(".credentials.json"))
        .chain([moved.path.join(".credentials.json")])
        .collect();
    // Providers read their files all the while: none may see a partial write.
    let stop = Arc::new(AtomicBool::new(false));
    let reads = Arc::new(AtomicUsize::new(0));
    let readers = (0..4)
        .map(|_| {
            let (paths, stop, reads) = (paths.clone(), stop.clone(), reads.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    for path in &paths {
                        let bytes = fs::read(path).unwrap();
                        let value: serde_json::Value = serde_json::from_slice(&bytes)
                            .expect("a view held a partially written auth file");
                        let token = value["claudeAiOauth"]["accessToken"].as_str().unwrap();
                        assert!(token.starts_with("synthetic-"));
                        reads.fetch_add(1, Ordering::SeqCst);
                    }
                }
            })
        })
        .collect::<Vec<_>>();
    let mut slowest = std::time::Duration::ZERO;
    for round in 0..20 {
        let token = format!("synthetic-rotated-{round}");
        let started = std::time::Instant::now();
        {
            let _lock = store.lock().unwrap();
            assert!(
                aswap::refresh::refresh_with(&store, one, 1000, true, |_, _| Ok(
                    json!({"access_token":token,"refresh_token":"synthetic-refresh-rotated","expires_in":3600})
                ))
                .unwrap()
            );
        }
        slowest = slowest.max(started.elapsed());
        // The refresh itself handed the token out: no run has ticked.
        for (_, view) in &runs {
            let now: serde_json::Value =
                store::json_read(&view.path.join(".credentials.json")).unwrap();
            assert_eq!(now["claudeAiOauth"]["accessToken"], token.as_str());
            assert_eq!(now["claudeAiOauth"]["refreshToken"], "aswap-owner-managed");
        }
    }
    stop.store(true, Ordering::SeqCst);
    for reader in readers {
        reader.join().unwrap();
    }
    assert!(reads.load(Ordering::SeqCst) > 0);
    eprintln!("slowest refresh with {VIEWS} views: {slowest:?}");
    let first: serde_json::Value = store::json_read(&native).unwrap();
    assert_eq!(first["mcpOAuth"], with_mcp["mcpOAuth"]);
    let kept: serde_json::Value = store::json_read(&moved.path.join(".credentials.json")).unwrap();
    assert_eq!(kept["claudeAiOauth"]["accessToken"], "synthetic-two-access");
    // The first view goes last: its native MCP login then reaches the others.
    for (_, view) in runs.iter_mut().rev() {
        let leftovers = fs::read_dir(&view.path)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains("handout")
            })
            .count();
        assert_eq!(leftovers, 0);
        // The run's own sync agrees with what it was handed: nothing to rewrite.
        let path = view.path.join(".credentials.json");
        let before = fs::metadata(&path).unwrap();
        view.sync(&store, one).unwrap();
        let after = fs::metadata(&path).unwrap();
        use std::os::unix::fs::MetadataExt;
        assert_eq!(before.ino(), after.ino(), "sync rewrote a handed-out view");
        assert!(!view.auth_changed(&store, one));
    }
}
