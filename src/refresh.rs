//! Provider-native OAuth refresh requests. Secrets never implement Debug/logging.
use crate::{
    provider::{self, Provider},
    store::{self, Account, Store},
    usage::Failure,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use std::io::Read;
// Public OAuth application identifiers from the providers' published clients.
pub const CLAUDE_CLIENT: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub const CODEX_CLIENT: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub fn expires_at(provider: Provider, auth: &Value) -> Option<u64> {
    if provider == Provider::Claude {
        auth.pointer("/claudeAiOauth/expiresAt")?
            .as_u64()
            .map(|t| t / 1000)
    } else {
        let token = auth.pointer("/tokens/access_token")?.as_str()?;
        let bytes = URL_SAFE_NO_PAD.decode(token.split('.').nth(1)?).ok()?;
        serde_json::from_slice::<Value>(&bytes)
            .ok()?
            .get("exp")?
            .as_u64()
    }
}
pub fn request_body(provider: Provider, auth: &Value) -> std::result::Result<Value, Failure> {
    let path = if provider == Provider::Claude {
        "/claudeAiOauth/refreshToken"
    } else {
        "/tokens/refresh_token"
    };
    let token = auth
        .pointer(path)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or(Failure::new("refresh-invalid-grant"))?;
    let mut body = json!({"grant_type":"refresh_token","refresh_token":token,"client_id":if provider==Provider::Claude{CLAUDE_CLIENT}else{CODEX_CLIENT}});
    if provider == Provider::Claude {
        let scopes = auth
            .pointer("/claudeAiOauth/scopes")
            .and_then(Value::as_array)
            .map(|s| s.iter().filter_map(Value::as_str).collect::<Vec<_>>())
            .filter(|s| !s.is_empty())
            .unwrap_or(vec![
                "user:profile",
                "user:inference",
                "user:sessions:claude_code",
                "user:mcp_servers",
                "user:file_upload",
            ]);
        body["scope"] = json!(scopes.join(" "));
    }
    Ok(body)
}
pub fn apply_response(
    provider: Provider,
    old: &Value,
    response: &Value,
    at: u64,
) -> std::result::Result<Value, Failure> {
    let access = response
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or(Failure::new("refresh-invalid-response"))?;
    let mut auth = old.clone();
    if provider == Provider::Claude {
        let oauth = auth
            .get_mut("claudeAiOauth")
            .and_then(Value::as_object_mut)
            .ok_or(Failure::new("refresh-invalid-response"))?;
        let seconds = response
            .get("expires_in")
            .and_then(Value::as_u64)
            .filter(|s| *s > 0)
            .ok_or(Failure::new("refresh-invalid-response"))?;
        oauth.insert("accessToken".into(), json!(access));
        oauth.insert(
            "expiresAt".into(),
            json!(at.saturating_add(seconds).saturating_mul(1000)),
        );
        if let Some(refresh) = response
            .get("refresh_token")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            oauth.insert("refreshToken".into(), json!(refresh));
        }
        if let Some(scope) = response.get("scope").and_then(Value::as_str) {
            oauth.insert(
                "scopes".into(),
                json!(scope.split_whitespace().collect::<Vec<_>>()),
            );
        }
    } else {
        let tokens = auth
            .get_mut("tokens")
            .and_then(Value::as_object_mut)
            .ok_or(Failure::new("refresh-invalid-response"))?;
        tokens.insert("access_token".into(), json!(access));
        for key in ["refresh_token", "id_token"] {
            if let Some(value) = response
                .get(key)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                tokens.insert(key.into(), json!(value));
            }
        }
        auth["last_refresh"] = json!(
            chrono::DateTime::from_timestamp(at as i64, 0)
                .ok_or(Failure::new("refresh-invalid-response"))?
                .to_rfc3339()
        );
    }
    Ok(auth)
}
fn request(provider: Provider, body: &Value) -> std::result::Result<Value, Failure> {
    let url = if provider == Provider::Claude {
        "https://platform.claude.com/v1/oauth/token"
    } else {
        "https://auth.openai.com/oauth/token"
    };
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| Failure::new("network"))?;
    // Both published clients use JSON for the default account token endpoint.
    let response = client
        .post(url)
        .json(body)
        .send()
        .map_err(|_| Failure::new("network"))?;
    let status = response.status().as_u16();
    let retry = crate::usage::parse_retry_after(
        response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok()),
        crate::now(),
    );
    let mut bytes = Vec::new();
    response
        .take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Failure::new("network"))?;
    if bytes.len() > 1024 * 1024 {
        return Err(Failure::new("refresh-invalid-response"));
    }
    let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    if !(200..300).contains(&status) {
        let mut failure = Failure::http(status, retry);
        failure.kind = match status {
            401 => "refresh-http-401",
            403 => "refresh-http-403",
            429 => "refresh-http-429",
            _ => "refresh-http-error",
        };
        if value.get("error").and_then(Value::as_str) == Some("invalid_grant") {
            failure.kind = "refresh-invalid-grant";
        }
        return Err(failure);
    }
    Ok(value)
}
/// Caller holds the store lock. Only the canonical owner can rotate tokens;
/// access-only live sessions never hold or compete for the refresh lock.
pub fn refresh_with<F>(
    store: &Store,
    account: &Account,
    at: u64,
    force: bool,
    request: F,
) -> std::result::Result<bool, Failure>
where
    F: FnOnce(Provider, &Value) -> std::result::Result<Value, Failure>,
{
    refresh_inner(store, account, at, force, false, request)
}
fn refresh_inner<F>(
    store: &Store,
    account: &Account,
    at: u64,
    force: bool,
    active: bool,
    request: F,
) -> std::result::Result<bool, Failure>
where
    F: FnOnce(Provider, &Value) -> std::result::Result<Value, Failure>,
{
    if crate::coordination::follower(store).map_err(|_| Failure::new("usage-unavailable"))?
        || (!active
            && !crate::coordination::is_owner(store)
                .map_err(|_| Failure::new("usage-unavailable"))?)
    {
        return Ok(false);
    }
    // Exclude refresh-capable clients from the previous aswap release during upgrade.
    let Ok(_legacy) = store.legacy_lock(account) else {
        return Ok(false);
    };
    let identity = crate::identity::read(
        account.provider,
        &store
            .profile(account.provider, &account.label)
            .map_err(|_| Failure::new("usage-unavailable"))?,
    )
    .unwrap_or_default();
    let _legacy_identity = if let Some(key) = identity.lock_key() {
        match store.legacy_lock(&Account {
            provider: account.provider,
            label: format!("identity-{key}"),
        }) {
            Ok(lock) => Some(lock),
            Err(_) => return Ok(false),
        }
    } else {
        None
    };
    let _refresh = store
        .refresh_lock(account)
        .map_err(|_| Failure::new("usage-unavailable"))?;
    let path = store
        .profile(account.provider, &account.label)
        .map_err(|_| Failure::new("usage-unavailable"))?
        .join(account.provider.auth_file());
    // Re-read only after acquiring the identity lock; another refresher may have won.
    let old: Value = store::json_read(&path).map_err(|_| Failure::new("usage-unavailable"))?;
    if account.provider == Provider::Codex
        && old.get("tokens").is_none_or(Value::is_null)
        && old.get("OPENAI_API_KEY").and_then(Value::as_str).is_some()
    {
        return Ok(false);
    }
    if !force
        && !expires_at(account.provider, &old)
            .is_some_and(|expiry| expiry <= at.saturating_add(300))
    {
        return Ok(false);
    }
    // A failed refresh must not become N identical requests from N live sessions.
    #[derive(Default, serde::Serialize, serde::Deserialize)]
    struct Backoff {
        next: u64,
        failures: u32,
    }
    let key = identity.lock_key().unwrap_or_else(|| account.label.clone());
    let gate = store
        .root
        .join(format!("{}-{key}.refresh-backoff.json", account.provider));
    let previous: Backoff = if gate.exists() {
        store::json_read(&gate).map_err(|_| Failure::new("usage-unavailable"))?
    } else {
        Backoff::default()
    };
    if !force && at < previous.next {
        return Ok(false);
    }
    let result = (|| {
        let body = request_body(account.provider, &old)?;
        let response = request(account.provider, &body)?;
        apply_response(account.provider, &old, &response, at)
    })();
    let auth = match result {
        Ok(auth) => auth,
        Err(error) => {
            let failures = previous.failures.saturating_add(1);
            let delay = 60u64
                .saturating_mul(1u64 << failures.saturating_sub(1).min(4))
                .min(900)
                .max(error.retry_after);
            store::json_write(
                &gate,
                &Backoff {
                    next: at.saturating_add(delay),
                    failures,
                },
            )
            .map_err(|_| Failure::new("usage-unavailable"))?;
            return Err(error);
        }
    };
    provider::validate_auth(
        account.provider,
        &serde_json::to_vec(&auth).map_err(|_| Failure::new("refresh-invalid-response"))?,
    )
    .map_err(|_| Failure::new("refresh-invalid-response"))?;
    store::json_write(&path, &auth).map_err(|_| Failure::new("usage-unavailable"))?;
    propagate(store, account, &auth).map_err(|_| Failure::new("usage-unavailable"))?;
    crate::usage::credentials_changed(store, account)
        .map_err(|_| Failure::new("usage-unavailable"))?;
    if gate.exists() {
        std::fs::remove_file(&gate).map_err(|_| Failure::new("usage-unavailable"))?;
    }
    Ok(true)
}
/// An active local run may refresh even when inactive-profile monitoring is off.
/// Followers still only pull from the designated owner.
pub fn for_run(store: &Store, account: &Account) -> std::result::Result<bool, Failure> {
    let _lock = store
        .lock()
        .map_err(|_| Failure::new("usage-unavailable"))?;
    refresh_inner(store, account, crate::now(), false, true, request)
}
pub fn refresh(
    store: &Store,
    account: &Account,
    at: u64,
    force: bool,
) -> std::result::Result<bool, Failure> {
    refresh_with(store, account, at, force, request)
}
/// Followers never possess refresh-capable auth, even while a provider is running.
pub fn access_only(provider: Provider, auth: &Value) -> Value {
    let mut auth = auth.clone();
    fn strip(value: &mut Value) {
        match value {
            Value::Object(map) => {
                map.retain(|k, _| {
                    !matches!(
                        k.to_ascii_lowercase().as_str(),
                        "refreshtoken" | "refresh_token"
                    )
                });
                for v in map.values_mut() {
                    strip(v);
                }
            }
            Value::Array(values) => {
                for v in values {
                    strip(v);
                }
            }
            _ => {}
        }
    }
    strip(&mut auth);
    if provider == Provider::Claude {
        if let Some(o) = auth.get_mut("claudeAiOauth").and_then(Value::as_object_mut) {
            o.remove("refreshToken");
        }
    } else if let Some(o) = auth.get_mut("tokens").and_then(Value::as_object_mut) {
        o.insert("refresh_token".into(), json!(""));
    }
    auth
}

/// Native Claude's 401 recovery first requires a nonempty refresh field, then
/// reloads a changed access token from disk. A public non-credential sentinel
/// enables that reload path without distributing a refresh-capable token.
/// Suppress proactive refresh of the sentinel; actual JWT validity is unchanged.
pub fn launch_auth(provider: Provider, auth: &Value) -> Value {
    let mut auth = access_only(provider, auth);
    if provider == Provider::Claude {
        if let Some(o) = auth.get_mut("claudeAiOauth").and_then(Value::as_object_mut) {
            o.insert("refreshToken".into(), json!("aswap-owner-managed"));
            if let Some(expiry) = o.get("expiresAt").and_then(Value::as_u64) {
                o.insert("expiresAt".into(), json!(expiry.saturating_add(86400000)));
            }
        }
    } else if auth
        .pointer("/tokens/access_token")
        .and_then(Value::as_str)
        .is_some_and(|token| !token.is_empty())
        && !auth
            .get("OPENAI_API_KEY")
            .and_then(Value::as_str)
            .is_some_and(|key| !key.is_empty())
    {
        auth["auth_mode"] = json!("chatgptAuthTokens");
    }
    auth
}

/// Caller holds store + account identity lease. Keep duplicate labels on the
/// same rotation while preserving unrelated per-profile credential extensions.
pub(crate) fn propagate(store: &Store, account: &Account, auth: &Value) -> crate::Result<()> {
    let aliases = crate::identity::duplicates(store, account, &store.load()?.accounts);
    for label in aliases {
        let other = Account {
            provider: account.provider,
            label,
        };
        let path = store
            .profile(other.provider, &other.label)?
            .join(other.provider.auth_file());
        let mut saved: Value = store::json_read(&path)?;
        let field = if account.provider == Provider::Claude {
            "claudeAiOauth"
        } else {
            "tokens"
        };
        if let Some(value) = auth.get(field) {
            saved[field] = value.clone();
        }
        if account.provider == Provider::Codex
            && let Some(value) = auth.get("last_refresh")
        {
            saved["last_refresh"] = value.clone();
        }
        store::json_write(&path, &saved)?;
        crate::usage::credentials_changed(store, &other)?;
    }
    Ok(())
}
