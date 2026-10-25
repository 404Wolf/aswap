use aswap::{
    identity::{from_values, same},
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
