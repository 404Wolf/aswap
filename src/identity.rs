//! Local identity hints are never persisted, serialized, or included in Debug.
use crate::{Result, provider::Provider, store};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::Value;
use std::path::Path;

#[derive(Default)]
pub struct Identity {
    pub email: Option<String>,
    pub plan: Option<String>,
    /// The legacy per-provider key: lock and backoff file names, sync
    /// conflict checks. Codex's is the workspace (`account_id`), which several
    /// users of one ChatGPT workspace share, so it is not an account identity.
    fingerprint: Option<Vec<u8>>,
    /// The real account, hashed and vendor-scoped, so one account reached
    /// through different programs matches: see [`same_account`].
    member: Option<Vec<u8>>,
    /// Codex logins whose token predates the membership claim: the user in
    /// the selected workspace. Matched only against another such pair.
    person: Option<Vec<u8>>,
}
impl Identity {
    pub fn known(&self) -> bool {
        self.fingerprint.is_some()
    }
    pub(crate) fn lock_key(&self) -> Option<String> {
        self.fingerprint.as_deref().map(hex)
    }
    /// The real account's key (hex), when the login names it with
    /// authority: Codex's `chatgpt_account_user_id` for the selected
    /// workspace, Claude's account in its organization. Never guessed.
    pub fn account_key(&self) -> Option<String> {
        self.member.as_deref().map(hex)
    }
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn digest<T: serde::Serialize>(value: &T) -> Vec<u8> {
    let bytes = serde_json::to_vec(value).expect("identity tuple encodes");
    ring::digest::digest(&ring::digest::SHA256, &bytes)
        .as_ref()
        .to_vec()
}
const OPENAI_AUTH: &str = "/https:~1~1api.openai.com~1auth";
/// A ChatGPT login's membership (the user in the selected workspace) as
/// Codex reads it (codex-rs/login token_data.rs
/// `parse_chatgpt_account_user_id`): `chatgpt_account_user_id` from the
/// access token, only when that token's `chatgpt_account_id` is the selected
/// workspace and both are clean strings. `chatgpt_account_id` alone is the
/// workspace ("Organization/workspace identifier"), shared by its users.
pub(crate) fn openai_member(access: &Value, selected: Option<&str>) -> Option<Vec<u8>> {
    let selected = selected.filter(|s| !s.is_empty() && s.trim() == *s)?;
    let auth = access.pointer(OPENAI_AUTH)?;
    if auth.get("chatgpt_account_id").and_then(Value::as_str) != Some(selected) {
        return None;
    }
    let member = auth
        .get("chatgpt_account_user_id")
        .and_then(Value::as_str)
        .filter(|m| !m.is_empty() && m.trim() == *m)?;
    Some(digest(&("openai", member)))
}
/// The user in the selected workspace, for tokens without the membership
/// claim: both the workspace and the user id, never `sub` or email.
pub(crate) fn openai_person(claims: &[&Value], selected: Option<&str>) -> Option<Vec<u8>> {
    let selected = selected.filter(|s| !s.is_empty() && s.trim() == *s)?;
    claims.iter().find_map(|c| {
        let auth = c.pointer(OPENAI_AUTH)?;
        if auth.get("chatgpt_account_id").and_then(Value::as_str) != Some(selected) {
            return None;
        }
        let user = ["chatgpt_user_id", "user_id"]
            .iter()
            .find_map(|k| auth.get(*k).and_then(Value::as_str))
            .filter(|u| !u.is_empty())?;
        Some(digest(&("openai-person", selected, user)))
    })
}
fn text(v: &Value, paths: &[&str]) -> Option<String> {
    paths.iter().find_map(|p| {
        v.pointer(p)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    })
}
/// A JWT's payload segment, as Codex accepts the envelope
/// (`decode_jwt_payload`): exactly three non-empty segments.
fn jwt_payload(jwt: &str) -> Option<&str> {
    let mut parts = jwt.split('.');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(h), Some(p), Some(s), None) if !h.is_empty() && !p.is_empty() && !s.is_empty() => {
            Some(p)
        }
        _ => None,
    }
}
fn claims(auth: &Value, key: &str) -> Value {
    auth.pointer(key)
        .and_then(Value::as_str)
        .and_then(jwt_payload)
        .and_then(|s| URL_SAFE_NO_PAD.decode(s).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null)
}
pub fn from_values(provider: Provider, auth: &Value, config: &Value) -> Identity {
    let (email, plan, account, organization) = match provider {
        Provider::Claude => (
            text(config, &["/oauthAccount/emailAddress"]),
            text(
                auth,
                &[
                    "/claudeAiOauth/subscriptionType",
                    "/claudeAiOauth/rateLimitTier",
                ],
            )
            .or_else(|| text(config, &["/oauthAccount/billingType"])),
            text(
                config,
                &["/oauthAccount/accountUuid", "/oauthAccount/emailAddress"],
            ),
            text(config, &["/oauthAccount/organizationUuid"]),
        ),
        Provider::Codex => {
            let id = claims(auth, "/tokens/id_token");
            let access = claims(auth, "/tokens/access_token");
            (
                text(&id, &["/email"]).or_else(|| text(auth, &["/email"])),
                text(&id, &["/https:~1~1api.openai.com~1auth/chatgpt_plan_type"]).or_else(|| {
                    text(
                        &access,
                        &["/https:~1~1api.openai.com~1auth/chatgpt_plan_type"],
                    )
                }),
                text(auth, &["/tokens/account_id"]).or_else(|| {
                    text(
                        &id,
                        &[
                            "/https:~1~1api.openai.com~1auth/chatgpt_account_id",
                            "/sub",
                            "/email",
                        ],
                    )
                }),
                None,
            )
        }
    };
    let (member, person) = match provider {
        // Claude's account in its organization (both from Claude's own
        // config), never the email.
        Provider::Claude => (
            text(config, &["/oauthAccount/accountUuid"]).map(|a| {
                digest(&(
                    "anthropic",
                    a,
                    text(config, &["/oauthAccount/organizationUuid"]),
                ))
            }),
            None,
        ),
        Provider::Codex => {
            let selected = text(auth, &["/tokens/account_id"]);
            let access = claims(auth, "/tokens/access_token");
            let id = claims(auth, "/tokens/id_token");
            (
                openai_member(&access, selected.as_deref()),
                openai_person(&[&access, &id], selected.as_deref()),
            )
        }
    };
    let fingerprint =
        account.map(|a| digest(&(provider.as_str(), a.trim().to_lowercase(), organization)));
    Identity {
        email,
        plan,
        fingerprint,
        member,
        person,
    }
}
pub fn read(provider: Provider, profile: &Path) -> Result<Identity> {
    let auth: Value = store::json_read(&profile.join(provider.auth_file()))?;
    let path = profile.join(".claude.json");
    let config = if provider == Provider::Claude && path.exists() {
        store::json_read(&path)?
    } else {
        Value::Null
    };
    Ok(from_values(provider, &auth, &config))
}
/// The legacy key matches (see [`Identity::fingerprint`]); not an account
/// match. Use [`same_account`] to decide that two logins share one account.
pub fn same(a: &Identity, b: &Identity) -> bool {
    a.fingerprint.is_some() && a.fingerprint == b.fingerprint
}
/// Whether two logins are one real account (one quota, one rotation): the
/// same membership, or, where either lacks it, the same user in the same
/// workspace on both. Uncertain is "no".
pub fn same_account(a: &Identity, b: &Identity) -> bool {
    match (&a.member, &b.member) {
        (Some(x), Some(y)) => x == y,
        _ => a.person.is_some() && a.person == b.person,
    }
}

/// Caller holds the store lock; only labels leave this helper.
pub fn duplicates(
    store: &crate::store::Store,
    account: &crate::store::Account,
    accounts: &[crate::store::Account],
) -> Vec<String> {
    let own = store
        .profile(account.provider, &account.label)
        .ok()
        .and_then(|p| read(account.provider, &p).ok())
        .unwrap_or_default();
    accounts
        .iter()
        .filter(|other| other.provider == account.provider && other.label != account.label)
        .filter_map(|other| {
            let id = store
                .profile(other.provider, &other.label)
                .ok()
                .and_then(|p| read(other.provider, &p).ok())
                .unwrap_or_default();
            same_account(&own, &id).then(|| other.label.clone())
        })
        .collect()
}
