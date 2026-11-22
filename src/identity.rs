//! Local identity hints are never persisted, serialized, or included in Debug.
use crate::{Result, provider::Provider, store};
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
pub(crate) fn digest<T: serde::Serialize>(value: &T) -> Vec<u8> {
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
pub fn from_values(provider: Provider, auth: &Value, config: &Value) -> Identity {
    let crate::provider::Claims {
        email,
        plan,
        account,
        organization,
        member,
        person,
    } = provider.spec().claims(auth, config);
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
    let config = match provider.spec().account_config().map(|f| profile.join(f)) {
        Some(path) if path.exists() => store::json_read(&path)?,
        _ => Value::Null,
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
/// The same real account in other programs (Codex and pi's ChatGPT login,
/// say): accounts of other providers with this one's account key. Only the
/// authoritative key links programs, never the older-token fallback. These
/// share quota, limits and resets, never credentials: each program keeps
/// its own login and refreshes it itself (`duplicates` stays per program).
/// Caller holds the store lock.
pub fn bindings(
    store: &crate::store::Store,
    account: &crate::store::Account,
    accounts: &[crate::store::Account],
) -> Vec<crate::store::Account> {
    let key = |a: &crate::store::Account| {
        store
            .profile(a.provider, &a.label)
            .ok()
            .and_then(|p| read(a.provider, &p).ok())
            .and_then(|id| id.account_key())
    };
    let Some(own) = key(account) else {
        return Vec::new();
    };
    accounts
        .iter()
        .filter(|other| other.provider != account.provider)
        .filter(|other| key(other).as_ref() == Some(&own))
        .cloned()
        .collect()
}
/// Every other account that is this one's real account: aliases in the
/// same program (`duplicates`) and bindings in others (`bindings`).
/// Caller holds the store lock.
pub fn siblings(
    store: &crate::store::Store,
    account: &crate::store::Account,
    accounts: &[crate::store::Account],
) -> Vec<crate::store::Account> {
    duplicates(store, account, accounts)
        .into_iter()
        .map(|label| crate::store::Account {
            provider: account.provider,
            label,
        })
        .chain(bindings(store, account, accounts))
        .collect()
}
