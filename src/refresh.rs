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
    // A rotation whose final write failed left the only valid refresh token
    // in the pending sidecar: install it before anything else.
    recover_pending(store, account, &path).map_err(|_| Failure::new("usage-unavailable"))?;
    // Re-read only after acquiring the identity lock; another refresher may have won.
    let old: Value = store::json_read(&path).map_err(|_| Failure::new("usage-unavailable"))?;
    // What a pending copy of a rotation replaces (see write_pending).
    let replaces = file_hash(&path);
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
    // The provider has already invalidated the old refresh token, so the
    // new login must survive a failed write: it goes to pending copies
    // first, and the next refresh installs one if the main write fails.
    write_pending(store, account, replaces, &auth);
    install(store, account, &path, &auth).map_err(|_| Failure::new("usage-unavailable"))?;
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
/// `<auth file>.pending` beside the login: private by name
/// (shared::private), never linked into a view.
fn pending_path(path: &std::path::Path) -> std::path::PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_owned();
    name.push(".pending");
    path.with_file_name(name)
}
/// A second pending copy in `$XDG_RUNTIME_DIR` (a tmpfs on systemd hosts),
/// so a full or failing data disk can't lose both. Named per store, so
/// two stores (a test's and the real one) never share it.
fn runtime_pending_path(store: &Store, account: &Account) -> Option<std::path::PathBuf> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .filter(|dir| dir.is_absolute() && dir.is_dir())?;
    let root = ring::digest::digest(
        &ring::digest::SHA256,
        store.root.as_os_str().as_encoded_bytes(),
    );
    let root: String = root.as_ref()[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Some(dir.join("aswap").join(format!(
        "{root}-{}-{}.pending.json",
        account.provider, account.label
    )))
}
fn pending_paths(store: &Store, account: &Account) -> Vec<std::path::PathBuf> {
    let profile = store.profile(account.provider, &account.label).ok();
    profile
        .map(|p| pending_path(&p.join(account.provider.auth_file())))
        .into_iter()
        .chain(runtime_pending_path(store, account))
        .collect()
}
/// A pending login: the rotated login and a hash of the login file it
/// replaces, so it is installed only over that file, never over a newer one.
#[derive(serde::Serialize, serde::Deserialize)]
struct Pending {
    replaces: String,
    auth: Value,
}
fn file_hash(path: &std::path::Path) -> Option<String> {
    let bytes = store::read_private(path).ok()?;
    Some(
        ring::digest::digest(&ring::digest::SHA256, &bytes)
            .as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
    )
}
/// Write both pending copies; either one is enough. `replaces` is the hash
/// of the login file the rotation started from.
fn write_pending(store: &Store, account: &Account, replaces: Option<String>, auth: &Value) {
    let Some(replaces) = replaces else {
        return;
    };
    let pending = Pending {
        replaces,
        auth: auth.clone(),
    };
    for copy in pending_paths(store, account) {
        let _ = store::json_write(&copy, &pending);
    }
}
/// Drop an account's pending logins: its login file is being replaced
/// (a new rotation, `add`, a live login preserved), so they are stale.
pub(crate) fn discard_pending(store: &Store, account: &Account) {
    for copy in pending_paths(store, account) {
        let _ = std::fs::remove_file(copy);
    }
}
/// Write a rotated login, hand it to every live run and alias, then drop
/// its pending copies (a failure to remove them is harmless: they no
/// longer match the login file).
fn install(
    store: &Store,
    account: &Account,
    path: &std::path::Path,
    auth: &Value,
) -> crate::Result<()> {
    store::json_write(path, auth)?;
    // The old access token is already revoked: hand the new one to every
    // live run now, before anything else, rather than on each run's tick.
    crate::shared::hand_out(store, account, auth);
    propagate(store, account, auth)?;
    crate::usage::credentials_changed(store, account)?;
    discard_pending(store, account);
    Ok(())
}
/// Install or drop an account's pending logins now, under its refresh lock
/// (the caller holds the store lock): before a rename moves the account.
pub(crate) fn settle_pending(store: &Store, account: &Account) -> crate::Result<()> {
    let _refresh = store.refresh_lock(account)?;
    let path = store
        .profile(account.provider, &account.label)?
        .join(account.provider.auth_file());
    recover_pending(store, account, &path)
}
/// Install a pending login left by a rotation whose final write failed,
/// if the login file is still the one it replaces; drop it otherwise.
fn recover_pending(store: &Store, account: &Account, path: &std::path::Path) -> crate::Result<()> {
    let copies: Vec<_> = pending_paths(store, account)
        .into_iter()
        .filter(|copy| copy.exists())
        .collect();
    if copies.is_empty() {
        return Ok(());
    }
    // A login file that can't be read (the disk still failing) proves
    // nothing: keep every copy, the tmpfs one above all, and retry later.
    let Some(current) = file_hash(path) else {
        return Err(crate::Error(
            "cannot read the login to recover its pending copy",
        ));
    };
    let current = Some(current);
    for copy in copies {
        let pending: Option<Pending> = store::json_read(&copy).ok();
        let usable = pending.filter(|p| {
            Some(&p.replaces) == current.as_ref()
                && serde_json::to_vec(&p.auth)
                    .is_ok_and(|bytes| provider::validate_auth(account.provider, &bytes).is_ok())
        });
        match usable {
            Some(pending) => return install(store, account, path, &pending.auth),
            None => {
                let _ = std::fs::remove_file(&copy);
            }
        }
    }
    Ok(())
}
/// An active local run may refresh even when inactive-profile monitoring is off.
/// Followers still only pull from the designated owner.
pub fn for_run(store: &Store, account: &Account) -> std::result::Result<bool, Failure> {
    let at = crate::now();
    // Every supervisor asks on its terminal thread every 30 s. Answer "not
    // due" without the store lock, so a slow lock holder never stalls
    // typing; refresh_inner decides again under the lock when it may be due.
    if !may_be_due(store, account, at) {
        return Ok(false);
    }
    let _lock = store
        .lock()
        .map_err(|_| Failure::new("usage-unavailable"))?;
    refresh_inner(store, account, at, false, true, request)
}
/// False only when an unforced refresh_inner would certainly do nothing:
/// the stored login (written atomically, so safe to read unlocked) has a
/// known expiry beyond the margin, or none at all.
fn may_be_due(store: &Store, account: &Account, at: u64) -> bool {
    let Ok(profile) = store.profile(account.provider, &account.label) else {
        return true;
    };
    let Ok(auth) = store::json_read::<Value>(&profile.join(account.provider.auth_file())) else {
        return true;
    };
    expires_at(account.provider, &auth).is_some_and(|expiry| expiry <= at.saturating_add(MARGIN))
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
#[cfg(test)]
mod pending_tests {
    use super::*;

    #[test]
    fn an_unreadable_login_keeps_its_pending_copy() {
        let t = tempfile::tempdir().unwrap();
        let store = Store::open(t.path().join("store")).unwrap();
        let account = Account {
            provider: Provider::Claude,
            label: "one".into(),
        };
        let path = store
            .profile(account.provider, &account.label)
            .unwrap()
            .join(account.provider.auth_file());
        // The login file can't be read (here: a directory in its place).
        std::fs::create_dir_all(&path).unwrap();
        let copy = pending_path(&path);
        store::json_write(
            &copy,
            &Pending {
                replaces: "synthetic-hash".into(),
                auth: serde_json::json!({"claudeAiOauth":{"accessToken":"synthetic"}}),
            },
        )
        .unwrap();
        assert!(recover_pending(&store, &account, &path).is_err());
        assert!(copy.exists(), "the pending copy was dropped");
        // Once the file reads and differs, the copy is stale and dropped.
        std::fs::remove_dir(&path).unwrap();
        std::fs::write(&path, b"{}").unwrap();
        recover_pending(&store, &account, &path).unwrap();
        assert!(!copy.exists());
    }
}
