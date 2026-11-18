//! Local identity hints are never persisted, serialized, or included in Debug.
use crate::{Result, provider::Provider, store};
use serde_json::Value;
use std::path::Path;

#[derive(Default)]
pub struct Identity {
    pub email: Option<String>,
    pub plan: Option<String>,
    fingerprint: Option<Vec<u8>>,
}
impl Identity {
    pub fn known(&self) -> bool {
        self.fingerprint.is_some()
    }
    pub(crate) fn lock_key(&self) -> Option<String> {
        self.fingerprint
            .as_ref()
            .map(|bytes| bytes.iter().map(|b| format!("{b:02x}")).collect())
    }
}
pub fn from_values(provider: Provider, auth: &Value, config: &Value) -> Identity {
    let crate::provider::Claims {
        email,
        plan,
        account,
        organization,
    } = provider.spec().claims(auth, config);
    let fingerprint = account.map(|a| {
        let bytes = serde_json::to_vec(&(provider.as_str(), a.trim().to_lowercase(), organization))
            .expect("identity tuple encodes");
        ring::digest::digest(&ring::digest::SHA256, &bytes)
            .as_ref()
            .to_vec()
    });
    Identity {
        email,
        plan,
        fingerprint,
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
pub fn same(a: &Identity, b: &Identity) -> bool {
    a.fingerprint.is_some() && a.fingerprint == b.fingerprint
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
            same(&own, &id).then(|| other.label.clone())
        })
        .collect()
}
