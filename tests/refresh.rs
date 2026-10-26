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
fn disabled_followers_and_running_profiles_never_rotate() {
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
    assert!(
        !refresh::refresh_with(&store, &a, 1000, true, |_, _| panic!(
            "running CLI owns rotation"
        ))
        .unwrap()
    );
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
fn duplicate_labels_share_the_running_lock_and_receive_one_rotation() {
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
    let lease = store.lease(&alias).unwrap();
    assert!(store.lease(&a).is_err());
    {
        let _lock = store.lock().unwrap();
        assert!(
            !refresh::refresh_with(&store, &a, 1000, true, |_, _| panic!(
                "a running alias owns rotation"
            ))
            .unwrap()
        );
    }
    drop(lease);
    let _lock = store.lock().unwrap();
    assert!(refresh::refresh_with(&store,&a,1000,true,|_,_|Ok(json!({"access_token":"synthetic-shared-new","refresh_token":"synthetic-shared-rotated","expires_in":3600}))).unwrap());
    let alias_auth: Value = store::json_read(&other.join(".credentials.json")).unwrap();
    assert_eq!(
        alias_auth["claudeAiOauth"]["refreshToken"],
        "synthetic-shared-rotated"
    );
    assert_eq!(alias_auth["syntheticAliasExtension"], true);
}
