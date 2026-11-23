use aswap::{
    coordination,
    provider::Provider,
    refresh,
    store::{self, Account, Store},
    usage::Failure,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};
fn seed() -> (tempfile::TempDir, Store, Account) {
    let t = tempfile::tempdir().unwrap();
    let store = Store::open(t.path().join("store")).unwrap();
    let a = Account {
        provider: Provider::Claude,
        label: "one".into(),
    };
    let mut state = store.load().unwrap();
    state.accounts.push(a.clone());
    store.save(&state).unwrap();
    store::json_write(&store.profile(a.provider,&a.label).unwrap().join(a.provider.auth_file()),&json!({"claudeAiOauth":{"accessToken":"synthetic-old","refreshToken":"synthetic-refresh-old","expiresAt":999000,"scopes":["user:profile","user:inference"],"subscriptionType":"max"},"syntheticExtra":true})).unwrap();
    (t, store, a)
}
#[test]
fn refresh_rotation_scope_expiry_and_atomic_write_under_lease() {
    let (_t, store, a) = seed();
    coordination::owner(&store).unwrap();
    let _lock = store.lock().unwrap();
    assert!(refresh::refresh_with(&store,&a,1000,false,|provider,body|{
        assert_eq!(provider,Provider::Claude);assert_eq!(body["client_id"],refresh::CLAUDE_CLIENT);assert_eq!(body["scope"],"user:profile user:inference");assert_eq!(body["refresh_token"],"synthetic-refresh-old");
        Ok(json!({"access_token":"synthetic-new","refresh_token":"synthetic-refresh-rotated","expires_in":3600,"scope":"user:profile user:inference"}))
    }).unwrap());
    let path = store
        .profile(a.provider, &a.label)
        .unwrap()
        .join(a.provider.auth_file());
    let value: Value = store::json_read(&path).unwrap();
    assert_eq!(value["claudeAiOauth"]["accessToken"], "synthetic-new");
    assert_eq!(
        value["claudeAiOauth"]["refreshToken"],
        "synthetic-refresh-rotated"
    );
    assert_eq!(value["claudeAiOauth"]["expiresAt"], 4600000);
    assert_eq!(value["syntheticExtra"], true);
    assert!(
        !refresh::refresh_with(&store, &a, 1001, false, |_, _| panic!(
            "fresh token must not rotate"
        ))
        .unwrap()
    );
    let before = std::fs::read(&path).unwrap();
    let failure = refresh::refresh_with(&store, &a, 1002, true, |_, _| {
        Err(Failure {
            kind: "refresh-invalid-grant",
            status: Some(400),
            retry_after: 0,
        })
    })
    .unwrap_err();
    assert_eq!(failure.kind, "refresh-invalid-grant");
    assert_eq!(std::fs::read(&path).unwrap(), before);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}
#[test]
fn disabled_monitors_do_not_rotate_but_owner_refreshes_running_profiles() {
    let (_t, store, a) = seed();
    assert!(
        !refresh::refresh_with(&store, &a, 1000, true, |_, _| panic!(
            "ownership must be explicit"
        ))
        .unwrap()
    );
    coordination::owner(&store).unwrap();
    let _lease = store.lease(&a).unwrap();
    let _lock = store.lock().unwrap();
    assert!(refresh::refresh_with(&store, &a, 1000, true, |_, _| {
        Ok(json!({"access_token":"synthetic-concurrent-fresh", "refresh_token":"synthetic-concurrent-refresh", "expires_in":3600}))
    }).unwrap());
}
#[test]
fn codex_flow_preserves_omitted_rotation_fields_and_owner_managed_launch_auth() {
    let token = format!(
        "synthetic.{}.synthetic",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&json!({"exp":1234})).unwrap())
    );
    let old = json!({"tokens":{"access_token":token,"refresh_token":"synthetic-refresh","id_token":"synthetic-id","account_id":"synthetic-account"}});
    assert_eq!(refresh::expires_at(Provider::Codex, &old), Some(1234));
    let body = refresh::request_body(Provider::Codex, &old).unwrap();
    assert_eq!(body["client_id"], refresh::CODEX_CLIENT);
    let after = refresh::apply_response(
        Provider::Codex,
        &old,
        &json!({"access_token":"synthetic-new-access"}),
        1000,
    )
    .unwrap();
    assert_eq!(after["tokens"]["refresh_token"], "synthetic-refresh");
    assert_eq!(after["tokens"]["account_id"], "synthetic-account");
    assert_eq!(after["tokens"]["id_token"], "synthetic-id");
    assert_eq!(
        refresh::access_only(Provider::Codex, &after)["tokens"]["refresh_token"],
        ""
    );
    assert_eq!(
        refresh::launch_auth(Provider::Codex, &after)["auth_mode"],
        "chatgptAuthTokens"
    );
    let api = json!({"auth_mode":"apikey","OPENAI_API_KEY":"synthetic-api-key"});
    assert_eq!(refresh::launch_auth(Provider::Codex, &api), api);
    let claude = json!({"claudeAiOauth":{"accessToken":"synthetic-access","refreshToken":"synthetic-real-refresh","expiresAt":1000000}});
    assert!(
        refresh::access_only(Provider::Claude, &claude)["claudeAiOauth"]
            .get("refreshToken")
            .is_none()
    );
    let launched = refresh::launch_auth(Provider::Claude, &claude);
    assert_ne!(
        launched["claudeAiOauth"]["refreshToken"],
        "synthetic-real-refresh"
    );
    assert!(
        launched["claudeAiOauth"]["refreshToken"]
            .as_str()
            .unwrap()
            .starts_with("aswap-")
    );
    assert_eq!(launched["claudeAiOauth"]["expiresAt"], 87400000);
    assert!(
        refresh::apply_response(
            Provider::Claude,
            &claude,
            &json!({"access_token":"synthetic-token"}),
            1000
        )
        .is_err()
    );
}

#[test]
fn duplicate_labels_allow_parallel_sessions_and_receive_one_rotation() {
    let (_t, store, a) = seed();
    let profile = store.profile(a.provider, &a.label).unwrap();
    store::json_write(
        &profile.join(".claude.json"),
        &json!({"oauthAccount":{"accountUuid":"synthetic-same-account"}}),
    )
    .unwrap();
    let alias = Account {
        provider: Provider::Claude,
        label: "duplicate".into(),
    };
    let mut state = store.load().unwrap();
    state.accounts.push(alias.clone());
    store.save(&state).unwrap();
    let other = store.profile(alias.provider, &alias.label).unwrap();
    store::json_write(
        &other.join(".claude.json"),
        &json!({"oauthAccount":{"accountUuid":"synthetic-same-account"}}),
    )
    .unwrap();
    let mut old: Value = store::json_read(&profile.join(".credentials.json")).unwrap();
    old["syntheticAliasExtension"] = json!(true);
    store::json_write(&other.join(".credentials.json"), &old).unwrap();
    coordination::owner(&store).unwrap();
    let _leases = [
        store.lease(&alias).unwrap(),
        store.lease(&a).unwrap(),
        store.lease(&a).unwrap(),
    ];
    let _lock = store.lock().unwrap();
    assert!(refresh::refresh_with(&store,&a,1000,false,|_,_|Ok(json!({"access_token":"synthetic-shared-new","refresh_token":"synthetic-shared-rotated","expires_in":3600}))).unwrap());
    assert!(
        !refresh::refresh_with(&store, &alias, 1001, false, |_, _| panic!(
            "alias must re-read rotated credentials"
        ))
        .unwrap()
    );
    let alias_auth: Value = store::json_read(&other.join(".credentials.json")).unwrap();
    assert_eq!(
        alias_auth["claudeAiOauth"]["refreshToken"],
        "synthetic-shared-rotated"
    );
    assert_eq!(alias_auth["syntheticAliasExtension"], true);
}

#[test]
fn many_concurrent_refreshers_re_read_one_rotation_while_sessions_are_live() {
    use std::sync::{
        Arc, Barrier,
        atomic::{AtomicUsize, Ordering},
    };
    let (_t, store, a) = seed();
    coordination::owner(&store).unwrap();
    let leases = (0..24)
        .map(|_| store.lease(&a).unwrap())
        .collect::<Vec<_>>();
    let barrier = Arc::new(Barrier::new(24));
    let calls = Arc::new(AtomicUsize::new(0));
    let active = Arc::new(AtomicUsize::new(0));
    let threads=(0..24).map(|_| {
        let root=store.root.clone();let a=a.clone();let barrier=barrier.clone();let calls=calls.clone();let active=active.clone();
        std::thread::spawn(move||{
            let store=Store::open(root).unwrap();barrier.wait();
            let _lock=store.lock().unwrap();
            refresh::refresh_with(&store,&a,1000,false,|_,body|{
                assert_eq!(active.fetch_add(1,Ordering::SeqCst),0);
                assert_eq!(body["refresh_token"],"synthetic-refresh-old");
                calls.fetch_add(1,Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(15));
                active.fetch_sub(1,Ordering::SeqCst);
                Ok(json!({"access_token":"synthetic-concurrent-new","refresh_token":"synthetic-concurrent-rotated","expires_in":3600}))
            }).unwrap()
        })
    }).collect::<Vec<_>>();
    let rotations = threads
        .into_iter()
        .map(|t| usize::from(t.join().unwrap()))
        .sum::<usize>();
    assert_eq!(rotations, 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(leases.len(), 24);
}

#[test]
fn failed_refresh_is_cooled_down_across_live_sessions() {
    let (_t, store, a) = seed();
    coordination::owner(&store).unwrap();
    let _live = (0..12)
        .map(|_| store.lease(&a).unwrap())
        .collect::<Vec<_>>();
    let _lock = store.lock().unwrap();
    let error = refresh::refresh_with(&store, &a, 1000, false, |_, _| {
        Err(Failure {
            kind: "http-429",
            status: Some(429),
            retry_after: 120,
        })
    })
    .unwrap_err();
    assert_eq!(error.status, Some(429));
    for at in 1001..1013 {
        assert!(
            !refresh::refresh_with(&store, &a, at, false, |_, _| panic!(
                "shared cooldown must prevent a stampede"
            ))
            .unwrap()
        );
    }
    assert!(
        refresh::refresh_with(&store, &a, 1120, false, |_, _| Ok(
            json!({"access_token":"synthetic-after-cooldown","expires_in":3600})
        ))
        .unwrap()
    );
}

#[test]
fn tokens_refresh_well_before_expiry_but_short_lived_ones_do_not_spin() {
    let (_t, store, a) = seed();
    coordination::owner(&store).unwrap();
    let _lock = store.lock().unwrap();
    // The seeded token expires at 999s.
    let early = 999 - refresh::MARGIN - 1;
    assert!(
        !refresh::refresh_with(&store, &a, early, false, |_, _| panic!(
            "a token outside the margin must not rotate"
        ))
        .unwrap()
    );
    let due = 999 - refresh::MARGIN;
    assert!(
        refresh::refresh_with(&store, &a, due, false, |_, _| Ok(
            json!({"access_token":"synthetic-short","expires_in":600})
        ))
        .unwrap()
    );
    // A 600s token is always inside the 900s margin, yet the next unforced
    // refresh waits half its lifetime instead of rotating on every check.
    for at in [due + 1, due + 299] {
        assert!(
            !refresh::refresh_with(&store, &a, at, false, |_, _| panic!(
                "a short-lived token must not rotate on every check"
            ))
            .unwrap()
        );
    }
    // A token that is useless at once (a one-second lifetime, or a clock
    // running ahead) still rotates at most once a minute.
    let at = due + 300;
    assert!(
        refresh::refresh_with(&store, &a, at, false, |_, _| Ok(
            json!({"access_token":"synthetic-instant","expires_in":1})
        ))
        .unwrap()
    );
    for later in [at + 1, at + refresh::MIN_INTERVAL - 1] {
        assert!(
            !refresh::refresh_with(&store, &a, later, false, |_, _| panic!(
                "an expired token must not rotate on every check"
            ))
            .unwrap()
        );
    }
    assert!(
        refresh::refresh_with(&store, &a, at + refresh::MIN_INTERVAL, false, |_, _| Ok(
            json!({"access_token":"synthetic-next","expires_in":3600})
        ))
        .unwrap()
    );
}

#[test]
fn a_rotated_login_survives_a_failed_write_and_is_installed_next_time() {
    let (_t, store, a) = seed();
    coordination::owner(&store).unwrap();
    let _lock = store.lock().unwrap();
    let path = store
        .profile(a.provider, &a.label)
        .unwrap()
        .join(a.provider.auth_file());
    let original = std::fs::read(&path).unwrap();
    // The token endpoint rotates the login; then the disk refuses the write
    // (here: the auth file's place is taken by a directory).
    let failed = refresh::refresh_with(&store, &a, 1000, false, |_, _| {
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        Ok(
            json!({"access_token":"synthetic-new","refresh_token":"synthetic-refresh-rotated","expires_in":3600}),
        )
    });
    assert!(failed.is_err());
    let pending = path.with_file_name(".credentials.json.pending");
    let kept: Value = store::json_read(&pending).unwrap();
    assert_eq!(
        kept["auth"]["claudeAiOauth"]["refreshToken"],
        "synthetic-refresh-rotated"
    );
    // The disk recovers. The next refresh installs the pending login first,
    // so it needn't (and can't) refresh with the invalidated old token.
    std::fs::remove_dir(&path).unwrap();
    std::fs::write(&path, &original).unwrap();
    assert!(
        !refresh::refresh_with(&store, &a, 1001, false, |_, _| panic!(
            "the old refresh token is no longer valid"
        ))
        .unwrap()
    );
    let installed: Value = store::json_read(&path).unwrap();
    assert_eq!(
        installed["claudeAiOauth"]["refreshToken"],
        "synthetic-refresh-rotated"
    );
    assert!(!pending.exists());
    // A normal rotation leaves no pending file behind.
    assert!(refresh::refresh_with(&store, &a, 9_999_999, true, |_, _| {
        Ok(json!({"access_token":"synthetic-newer","refresh_token":"synthetic-refresh-3","expires_in":3600}))
    })
    .unwrap());
    assert!(!pending.exists());
}

fn sha256_hex(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[test]
fn a_stale_pending_login_never_replaces_a_newer_one() {
    let (_t, store, a) = seed();
    coordination::owner(&store).unwrap();
    let _lock = store.lock().unwrap();
    let path = store
        .profile(a.provider, &a.label)
        .unwrap()
        .join(a.provider.auth_file());
    // A rotation crashed after its main write (or couldn't remove its
    // pending copy): the copy remains, made for the login file before.
    let pending = path.with_file_name(".credentials.json.pending");
    store::json_write(
        &pending,
        &json!({"replaces": sha256_hex(&std::fs::read(&path).unwrap()),
                "auth": {"claudeAiOauth":{"accessToken":"synthetic-stale","refreshToken":"synthetic-refresh-stale","expiresAt":999000}}}),
    )
    .unwrap();
    // Since then the login file holds a newer login (another rotation, or
    // `add` over the label).
    store::json_write(
        &path,
        &json!({"claudeAiOauth":{"accessToken":"synthetic-newer","refreshToken":"synthetic-refresh-newer","expiresAt":9_999_999_000u64}}),
    )
    .unwrap();
    assert!(!refresh::refresh_with(&store, &a, 1000, false, |_, _| panic!("not due")).unwrap());
    let now: Value = store::json_read(&path).unwrap();
    assert_eq!(
        now["claudeAiOauth"]["refreshToken"],
        "synthetic-refresh-newer"
    );
    assert!(!pending.exists(), "the stale pending login is dropped");
}

/// Runs `name` in a child test process with its own XDG_RUNTIME_DIR.
fn with_runtime_dir(name: &str) {
    let t = tempfile::tempdir().unwrap();
    let runtime = t.path().join("run");
    std::fs::create_dir(&runtime).unwrap();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env("ASWAP_REFRESH_CHILD", name)
        .env("XDG_RUNTIME_DIR", &runtime)
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
fn a_rotation_survives_when_its_disk_refuses_the_pending_copy_too() {
    with_runtime_dir("pending_runtime_copy_child");
}
#[test]
fn pending_runtime_copy_child() {
    if std::env::var("ASWAP_REFRESH_CHILD").as_deref() != Ok("pending_runtime_copy_child") {
        return;
    }
    let (_t, store, a) = seed();
    coordination::owner(&store).unwrap();
    let _lock = store.lock().unwrap();
    let path = store
        .profile(a.provider, &a.label)
        .unwrap()
        .join(a.provider.auth_file());
    let original = std::fs::read(&path).unwrap();
    // The data disk refuses both the pending copy beside the login and the
    // login itself (each place taken by a directory).
    let pending = path.with_file_name(".credentials.json.pending");
    std::fs::create_dir(&pending).unwrap();
    let failed = refresh::refresh_with(&store, &a, 1000, false, |_, _| {
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        Ok(
            json!({"access_token":"synthetic-new","refresh_token":"synthetic-refresh-rotated","expires_in":3600}),
        )
    });
    assert!(failed.is_err());
    // The copy under XDG_RUNTIME_DIR has it.
    let runtime = std::fs::read_dir(
        std::path::PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap()).join("aswap"),
    )
    .unwrap()
    .map(|e| e.unwrap().path())
    .find(|p| p.to_string_lossy().ends_with("-claude-one.pending.json"))
    .unwrap();
    let kept: Value = store::json_read(&runtime).unwrap();
    assert_eq!(
        kept["auth"]["claudeAiOauth"]["refreshToken"],
        "synthetic-refresh-rotated"
    );
    // The disk recovers; the next refresh installs it.
    std::fs::remove_dir(&pending).unwrap();
    std::fs::remove_dir(&path).unwrap();
    std::fs::write(&path, &original).unwrap();
    assert!(!refresh::refresh_with(&store, &a, 1001, false, |_, _| panic!("no request")).unwrap());
    let installed: Value = store::json_read(&path).unwrap();
    assert_eq!(
        installed["claudeAiOauth"]["refreshToken"],
        "synthetic-refresh-rotated"
    );
    assert!(!runtime.exists());
}

#[test]
fn pending_copies_are_kept_while_the_login_cannot_be_read() {
    with_runtime_dir("pending_unreadable_child");
}
#[test]
fn pending_unreadable_child() {
    if std::env::var("ASWAP_REFRESH_CHILD").as_deref() != Ok("pending_unreadable_child") {
        return;
    }
    let (_t, store, a) = seed();
    coordination::owner(&store).unwrap();
    let _lock = store.lock().unwrap();
    let path = store
        .profile(a.provider, &a.label)
        .unwrap()
        .join(a.provider.auth_file());
    let original = std::fs::read(&path).unwrap();
    // A rotation whose login write failed leaves both pending copies.
    let failed = refresh::refresh_with(&store, &a, 1000, false, |_, _| {
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        Ok(
            json!({"access_token":"synthetic-new","refresh_token":"synthetic-refresh-rotated","expires_in":3600}),
        )
    });
    assert!(failed.is_err());
    let runtime_dir =
        std::path::PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap()).join("aswap");
    let copies = || {
        std::fs::read_dir(&runtime_dir).unwrap().count()
            + usize::from(path.with_file_name(".credentials.json.pending").exists())
    };
    assert_eq!(copies(), 2);
    // The login still can't be read: nothing is dropped, nothing installed.
    assert!(refresh::refresh_with(&store, &a, 1001, false, |_, _| panic!("no request")).is_err());
    assert_eq!(copies(), 2);
    // Once it reads again, the copy is installed and both are dropped.
    std::fs::remove_dir(&path).unwrap();
    std::fs::write(&path, &original).unwrap();
    assert!(!refresh::refresh_with(&store, &a, 1002, false, |_, _| panic!("no request")).unwrap());
    let installed: Value = store::json_read(&path).unwrap();
    assert_eq!(
        installed["claudeAiOauth"]["refreshToken"],
        "synthetic-refresh-rotated"
    );
    assert_eq!(copies(), 0);
}

#[test]
fn a_rename_never_loses_a_pending_login() {
    let (_t, store, a) = seed();
    coordination::owner(&store).unwrap();
    let path = store
        .profile(a.provider, &a.label)
        .unwrap()
        .join(a.provider.auth_file());
    let original = std::fs::read(&path).unwrap();
    {
        let _lock = store.lock().unwrap();
        let failed = refresh::refresh_with(&store, &a, 1000, false, |_, _| {
            std::fs::remove_file(&path).unwrap();
            std::fs::create_dir(&path).unwrap();
            Ok(
                json!({"access_token":"synthetic-new","refresh_token":"synthetic-refresh-rotated","expires_in":3600}),
            )
        });
        assert!(failed.is_err());
        std::fs::remove_dir(&path).unwrap();
        std::fs::write(&path, &original).unwrap();
    }
    let pending = path.with_file_name(".credentials.json.pending");
    assert!(pending.exists());
    // A rename that fails its preflight (the new label is taken) keeps it.
    let mut state = store.load().unwrap();
    state.accounts.push(Account {
        provider: a.provider,
        label: "taken".into(),
    });
    store.save(&state).unwrap();
    assert!(aswap::rename::rename(&store, a.provider, "one", "taken").is_err());
    assert!(
        pending.exists(),
        "a failed rename dropped the pending login"
    );
    // A rename that goes ahead installs it first: the renamed account has
    // the rotated login and no copy is left under either label.
    aswap::rename::rename(&store, a.provider, "one", "renamed").unwrap();
    let moved = store
        .profile(a.provider, "renamed")
        .unwrap()
        .join(a.provider.auth_file());
    let login: Value = store::json_read(&moved).unwrap();
    assert_eq!(
        login["claudeAiOauth"]["refreshToken"],
        "synthetic-refresh-rotated"
    );
    assert!(!moved.with_file_name(".credentials.json.pending").exists());
}

#[test]
fn the_token_request_runs_without_the_store_lock() {
    // agy.4 review (4): a supervisor's terminal bookkeeping must never wait
    // on another process's token request.
    use fs2::FileExt;
    let (_t, store, a) = seed();
    coordination::owner(&store).unwrap();
    let lock_path = store.root.join("store.lock");
    // As a supervisor asks: without holding the store lock itself.
    let rotated = refresh::for_run_with(&store, &a, 1000, |_, _| {
        // Another process can take the store lock during the request.
        let other = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        assert!(other.try_lock_exclusive().is_ok(), "store lock held across the request");
        fs2::FileExt::unlock(&other).unwrap();
        Ok(json!({"access_token":"synthetic-new","refresh_token":"synthetic-refresh-rotated","expires_in":3600}))
    })
    .unwrap();
    assert!(rotated);
    let saved: Value = store::json_read(
        &store
            .profile(a.provider, &a.label)
            .unwrap()
            .join(a.provider.auth_file()),
    )
    .unwrap();
    assert_eq!(
        saved["claudeAiOauth"]["refreshToken"],
        "synthetic-refresh-rotated"
    );
}
