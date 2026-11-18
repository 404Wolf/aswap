//! Provider-native OAuth refresh requests. Secrets never implement Debug/logging.
use crate::{
    provider::{self, Encoding, Provider, Refresh},
    store::{self, Account, Store},
    usage::Failure,
};
use serde_json::Value;
use std::io::Read;
// Public OAuth application identifiers from the providers' published clients.
pub use crate::provider::{CLAUDE_CLIENT, CODEX_CLIENT};
/// Refresh this many seconds before a token expires. A rotation revokes the
/// old access token at once however early it happens, so the margin is room
/// for the failure backoff (1, 2, 4 then 8 minutes) to retry before expiry.
pub const MARGIN: u64 = 900;
/// The shortest time between two unforced refreshes of one login.
pub const MIN_INTERVAL: u64 = 60;
pub fn expires_at(provider: Provider, auth: &Value) -> Option<u64> {
    provider.spec().expires_at(auth)
}
pub fn request_body(provider: Provider, auth: &Value) -> std::result::Result<Value, Failure> {
    provider.spec().refresh(auth).map(|r| r.body)
}
pub fn apply_response(
    provider: Provider,
    old: &Value,
    response: &Value,
    at: u64,
) -> std::result::Result<Value, Failure> {
    provider.spec().apply_refresh(old, response, at)
}
fn request(_provider: Provider, refresh: &Refresh) -> std::result::Result<Value, Failure> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| Failure::new("network"))?;
    let request = client.post(refresh.url);
    let request = match refresh.encoding {
        Encoding::Json => request.json(&refresh.body),
        Encoding::Form => request.form(&refresh.body),
    };
    let response = request.send().map_err(|_| Failure::new("network"))?;
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
    refresh_inner(store, account, at, force, false, |provider, refresh| {
        request(provider, &refresh.body)
    })
}
/// `refresh_with`, with the whole request (URL, encoding and body) given to
/// `request`, so a test can check each provider's request end to end.
pub fn refresh_request_with<F>(
    store: &Store,
    account: &Account,
    at: u64,
    force: bool,
    request: F,
) -> std::result::Result<bool, Failure>
where
    F: FnOnce(Provider, &Refresh) -> std::result::Result<Value, Failure>,
{
    refresh_inner(store, account, at, force, false, request)
}
pub(crate) fn refresh_inner<F>(
    store: &Store,
    account: &Account,
    at: u64,
    force: bool,
    active: bool,
    request: F,
) -> std::result::Result<bool, Failure>
where
    F: FnOnce(Provider, &Refresh) -> std::result::Result<Value, Failure>,
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
    if !account.provider.spec().refreshable(&old) {
        return Ok(false);
    }
    if !force
        && !expires_at(account.provider, &old)
            .is_some_and(|expiry| expiry <= at.saturating_add(MARGIN))
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
        let refresh = account.provider.spec().refresh(&old)?;
        let response = request(account.provider, &refresh)?;
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
    // The old access token is already revoked: hand the new one to every
    // live run now, before anything else, rather than on each run's tick.
    crate::shared::hand_out(store, account, &auth);
    propagate(store, account, &auth).map_err(|_| Failure::new("usage-unavailable"))?;
    crate::usage::credentials_changed(store, account)
        .map_err(|_| Failure::new("usage-unavailable"))?;
    // A token that lives under twice the margin would be due again soon, or
    // on every check: the next unforced refresh waits half its lifetime, and
    // never less than a minute, even for a token that is already expired.
    let lifetime = expires_at(account.provider, &auth).map(|expiry| expiry.saturating_sub(at));
    if let Some(lifetime) = lifetime.filter(|lifetime| *lifetime < MARGIN.saturating_mul(2)) {
        store::json_write(
            &gate,
            &Backoff {
                next: at.saturating_add((lifetime / 2).max(MIN_INTERVAL)),
                failures: 0,
            },
        )
        .map_err(|_| Failure::new("usage-unavailable"))?;
    } else if gate.exists() {
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
    refresh_inner(store, account, at, force, false, request)
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
    provider.spec().strip_refresh(&mut auth);
    auth
}

/// The access-only login a launch view carries (see `Spec::launch_auth`).
pub fn launch_auth(provider: Provider, auth: &Value) -> Value {
    provider.spec().launch_auth(access_only(provider, auth))
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
        for field in account.provider.spec().login_fields() {
            if let Some(value) = auth.get(*field) {
                saved[*field] = value.clone();
            }
        }
        store::json_write(&path, &saved)?;
        crate::shared::hand_out(store, &other, &saved);
        crate::usage::credentials_changed(store, &other)?;
    }
    Ok(())
}
