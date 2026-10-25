//! Local identity hints are never persisted, serialized, or included in Debug.
use crate::{Result, provider::Provider, store};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::Value;
use std::path::Path;

#[derive(Default)]
pub struct Identity {
    pub email: Option<String>,
    pub plan: Option<String>,
    fingerprint: Option<Vec<u8>>,
}
fn text(v: &Value, paths: &[&str]) -> Option<String> {
    paths.iter().find_map(|p| {
        v.pointer(p)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    })
}
fn claims(auth: &Value, key: &str) -> Value {
    auth.pointer(key)
        .and_then(Value::as_str)
        .and_then(|s| s.split('.').nth(1))
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
    let path = profile.join(".claude.json");
    let config = if provider == Provider::Claude && path.exists() {
        store::json_read(&path)?
    } else {
        Value::Null
    };
    Ok(from_values(provider, &auth, &config))
}
pub fn same(a: &Identity, b: &Identity) -> bool {
    a.fingerprint.is_some() && a.fingerprint == b.fingerprint
}
