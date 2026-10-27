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
