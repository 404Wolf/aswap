use aswap::{
    identity::{duplicates, from_values, same, same_account},
    provider::Provider,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::json;
#[test]
fn claude_identity_survives_rotation_and_separates_accounts_and_organizations() {
    let auth =
        json!({"claudeAiOauth":{"accessToken":"synthetic-original", "subscriptionType":"max"}});
    let config = json!({"oauthAccount":{"accountUuid":"synthetic-a", "organizationUuid":"synthetic-org", "emailAddress":"alice@example.com"}});
    let id = from_values(Provider::Claude, &auth, &config);
    assert_eq!(id.email.as_deref(), Some("alice@example.com"));
    assert_eq!(id.plan.as_deref(), Some("max"));
    assert!(same(
        &id,
        &from_values(
            Provider::Claude,
            &json!({"claudeAiOauth":{"accessToken":"synthetic-rotated"}}),
            &config
        )
    ));
    for changed in [
        json!({"oauthAccount":{"accountUuid":"synthetic-b", "organizationUuid":"synthetic-org"}}),
        json!({"oauthAccount":{"accountUuid":"synthetic-a", "organizationUuid":"synthetic-other"}}),
    ] {
        assert!(!same(&id, &from_values(Provider::Claude, &auth, &changed)));
    }
    let unknown = from_values(Provider::Claude, &auth, &json!({}));
    assert!(
        !same(&unknown, &unknown),
        "opaque tokens do not stand in for account identity"
    );
}
#[test]
fn codex_claims_and_account_id() {
    let claims = json!({"sub":"synthetic-user", "email":"bob@example.com", "https://api.openai.com/auth":{"chatgpt_plan_type":"pro", "chatgpt_account_id":"synthetic-id"}});
    let jwt = format!(
        "synthetic.{}.synthetic",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
    );
    let auth = json!({"tokens":{"account_id":"synthetic-id", "id_token":jwt, "access_token":"synthetic-access"}});
    let id = from_values(Provider::Codex, &auth, &json!({}));
    assert_eq!(id.email.as_deref(), Some("bob@example.com"));
    assert_eq!(id.plan.as_deref(), Some("pro"));
    assert!(same(
        &id,
        &from_values(
            Provider::Codex,
            &json!({"tokens":{"account_id":"synthetic-id"}}),
            &json!({})
        )
    ));
    for bad in ["", "invalid", "a.***.b", "a.b.b"] {
        let id = from_values(
            Provider::Codex,
            &json!({"tokens":{"id_token":bad}}),
            &json!({}),
        );
        assert!(id.email.is_none());
    }
}

fn jwt(auth: serde_json::Value) -> String {
    let claims = json!({"sub": "synthetic-sub", "https://api.openai.com/auth": auth});
    format!(
        "synthetic.{}.synthetic",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
    )
}
fn codex(workspace: &str, access: serde_json::Value) -> aswap::identity::Identity {
    from_values(
        Provider::Codex,
        &json!({"tokens":{"account_id":workspace, "access_token":jwt(access), "id_token":jwt(json!({}))}}),
        &json!({}),
    )
}

#[test]
fn teammates_in_one_chatgpt_workspace_are_not_one_account() {
    // `chatgpt_account_id` is the workspace (Codex: "Organization/workspace
    // identifier"); each member has its own `chatgpt_account_user_id`.
    let alice = codex(
        "synthetic-workspace",
        json!({"chatgpt_account_id":"synthetic-workspace","chatgpt_account_user_id":"synthetic-member-a","chatgpt_user_id":"synthetic-user-a"}),
    );
    let bob = codex(
        "synthetic-workspace",
        json!({"chatgpt_account_id":"synthetic-workspace","chatgpt_account_user_id":"synthetic-member-b","chatgpt_user_id":"synthetic-user-b"}),
    );
    assert!(same(&alice, &bob), "the legacy key is the workspace");
    assert!(!same_account(&alice, &bob));
    assert_ne!(alice.account_key(), bob.account_key());
    // Alice's login rotated: still Alice.
    let rotated = codex(
        "synthetic-workspace",
        json!({"chatgpt_account_id":"synthetic-workspace","chatgpt_account_user_id":"synthetic-member-a","chatgpt_user_id":"synthetic-user-a","synthetic":"rotated"}),
    );
    assert!(same_account(&alice, &rotated));
    assert!(alice.account_key().is_some() && alice.account_key() == rotated.account_key());
}

#[test]
fn a_membership_counts_only_for_the_selected_workspace_and_never_from_guesses() {
    let member = |workspace: &str, claim_workspace: &str, member: serde_json::Value| {
        codex(
            workspace,
            json!({"chatgpt_account_id":claim_workspace,"chatgpt_account_user_id":member}),
        )
    };
    let real = member("synthetic-a", "synthetic-a", json!("synthetic-member"));
    for unscoped in [
        member("synthetic-a", "synthetic-b", json!("synthetic-member")),
        member("synthetic-a", "synthetic-a", json!("")),
        member("synthetic-a", "synthetic-a", json!(" synthetic-member ")),
        member("synthetic-a", "synthetic-a", json!(123)),
        member("", "", json!("synthetic-member")),
    ] {
        assert!(unscoped.account_key().is_none());
        assert!(!same_account(&real, &unscoped));
    }
    // Only in the id token (Codex reads it from the access token): unknown.
    let id_only = from_values(
        Provider::Codex,
        &json!({"tokens":{"account_id":"synthetic-a","access_token":"opaque","id_token":jwt(json!({"chatgpt_account_id":"synthetic-a","chatgpt_account_user_id":"synthetic-member"}))}}),
        &json!({}),
    );
    assert!(id_only.account_key().is_none());
    // A workspace alone, `sub` or email: never one account.
    let bare = from_values(
        Provider::Codex,
        &json!({"tokens":{"account_id":"synthetic-a"}}),
        &json!({}),
    );
    assert!(!same_account(&bare, &bare));
}

#[test]
fn tokens_without_the_membership_claim_match_on_the_workspace_user() {
    let old = |user: &str| {
        codex(
            "synthetic-workspace",
            json!({"chatgpt_account_id":"synthetic-workspace","chatgpt_user_id":user}),
        )
    };
    assert!(same_account(
        &old("synthetic-user-a"),
        &old("synthetic-user-a")
    ));
    assert!(!same_account(
        &old("synthetic-user-a"),
        &old("synthetic-user-b")
    ));
    assert!(
        old("synthetic-user-a").account_key().is_none(),
        "not an authoritative key"
    );
    // One side with the membership, the other without: the user pair decides.
    let new = codex(
        "synthetic-workspace",
        json!({"chatgpt_account_id":"synthetic-workspace","chatgpt_account_user_id":"synthetic-member-a","chatgpt_user_id":"synthetic-user-a"}),
    );
    assert!(same_account(&new, &old("synthetic-user-a")));
    assert!(!same_account(&new, &old("synthetic-user-b")));
}

#[test]
fn claude_accounts_match_on_account_and_organization_never_on_email() {
    let claude = |config: serde_json::Value| {
        from_values(
            Provider::Claude,
            &json!({"claudeAiOauth":{"accessToken":"synthetic"}}),
            &config,
        )
    };
    let a = claude(
        json!({"oauthAccount":{"accountUuid":"synthetic-a","organizationUuid":"synthetic-org"}}),
    );
    assert!(same_account(
        &a,
        &claude(
            json!({"oauthAccount":{"accountUuid":"synthetic-a","organizationUuid":"synthetic-org"}})
        )
    ));
    assert!(!same_account(
        &a,
        &claude(
            json!({"oauthAccount":{"accountUuid":"synthetic-a","organizationUuid":"synthetic-other"}})
        )
    ));
    let email = claude(json!({"oauthAccount":{"emailAddress":"alice@example.com"}}));
    assert!(same(&email, &email), "the legacy key falls back to email");
    assert!(!same_account(&email, &email));
    assert!(email.account_key().is_none());
}

#[test]
fn teammates_are_not_duplicates_so_one_rotation_never_overwrites_the_other() {
    use aswap::store::{self, Account, Store};
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("data/aswap")).unwrap();
    let mut state = store.load().unwrap();
    let write = |label: &str, member: &str| {
        let account = Account {
            provider: Provider::Codex,
            label: label.into(),
        };
        let profile = store.profile(account.provider, &account.label).unwrap();
        std::fs::create_dir_all(&profile).unwrap();
        store::json_write(
            &profile.join("auth.json"),
            &json!({"tokens":{"account_id":"synthetic-workspace","access_token":jwt(json!({"chatgpt_account_id":"synthetic-workspace","chatgpt_account_user_id":member})),"refresh_token":"synthetic-refresh"}}),
        )
        .unwrap();
        account
    };
    let a = write("alice", "synthetic-member-a");
    let b = write("bob", "synthetic-member-b");
    let a2 = write("alice-too", "synthetic-member-a");
    state.accounts.extend([a.clone(), b, a2]);
    store.save(&state).unwrap();
    let _lock = store.lock().unwrap();
    assert_eq!(duplicates(&store, &a, &state.accounts), ["alice-too"]);
}

#[test]
fn a_pi_chatgpt_login_names_the_same_account_as_codex_and_never_a_teammate() {
    let member = |member: &str| json!({"chatgpt_account_id":"synthetic-workspace","chatgpt_account_user_id":member});
    let pi = |kind: &str, member_claims: serde_json::Value| {
        from_values(
            Provider::Pi,
            &json!({kind: {"type":"oauth","access":jwt(member_claims),"refresh":"synthetic-refresh",
                "expires":4_000_000_000_000u64,"accountId":"synthetic-workspace"}}),
            &json!({}),
        )
    };
    let codex_a = codex("synthetic-workspace", member("synthetic-member-a"));
    let pi_a = pi("openai-codex", member("synthetic-member-a"));
    assert!(pi_a.account_key().is_some());
    assert_eq!(
        pi_a.account_key(),
        codex_a.account_key(),
        "vendor-scoped, not per program"
    );
    assert_ne!(
        pi("openai-codex", member("synthetic-member-b")).account_key(),
        codex_a.account_key()
    );
    // Sign in with ChatGPT (api.openai.com) tokens are not matched.
    assert!(
        pi("openai", member("synthetic-member-a"))
            .account_key()
            .is_none()
    );
}
