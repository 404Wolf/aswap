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
/// Refresh this many seconds before a token expires. A rotation revokes the
/// old access token at once however early it happens, so the margin is room
/// for the failure backoff (1, 2, 4 then 8 minutes) to retry before expiry.
pub const MARGIN: u64 = 900;
/// The shortest time between two unforced refreshes of one login.
pub const MIN_INTERVAL: u64 = 60;
pub fn expires_at(provider: Provider, auth: &Value) -> Option<u64> {
    if provider == Provider::Agy {
        return crate::agy::expires_at(auth);
    }
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
/// Only the canonical owner can rotate tokens; access-only live sessions
/// never hold or compete for the refresh lock. The store lock is taken here
/// only around reads and writes, never across the token request: a caller
/// that already holds it (tests) keeps it throughout instead.
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
pub(crate) fn refresh_inner<F>(
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
    // agy refreshes its own login; aswap never calls Google's token endpoint
    // (see crate::agy).
    // 0.2.1 ignores our pending sidecars. Leave canonical rotation to the
    // existing owner while its ordinary processes share this store.
    if account.provider == Provider::Agy {
        return Ok(false);
    }
    // Hold this guard across the request and every write. If admission is
    // underway, skip before touching auth, even if our caller has store.lock.
    let Some(_admission) = store
        .try_admission_lock(false)
        .map_err(|_| Failure::new("usage-unavailable"))?
    else {
        return Ok(false);
    };
    if store.legacy_compatibility() {
        return Ok(false);
    }
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
    // Lock order, everywhere: an account's refresh lock BEFORE the store
    // lock, never the reverse (holders of the store lock only ever try the
    // refresh lock; see Store::try_refresh_lock).
    let _refresh = store
        .refresh_lock(account)
        .map_err(|_| Failure::new("usage-unavailable"))?;
    // The store lock for the reads and checks below, released before the
    // network request unless the caller holds it.
    let mut store_lock = if store::lock_held() {
        None
    } else {
        Some(
            store
                .lock()
                .map_err(|_| Failure::new("usage-unavailable"))?,
        )
    };
    let path = store
        .profile(account.provider, &account.label)
        .map_err(|_| Failure::new("usage-unavailable"))?
        .join(account.provider.auth_file());
    // A rotation whose final write failed left the only valid refresh token
    // in the pending sidecar: install it before anything else.
    recover_pending(store, account).map_err(|_| Failure::new("usage-unavailable"))?;
    // Re-read only after acquiring the identity lock; another refresher may have won.
    let old: Value = store::json_read(&path).map_err(|_| Failure::new("usage-unavailable"))?;
    // What a pending copy of a rotation replaces (see write_pending).
    let replaces = file_hash(&path);
    if account.provider == Provider::Codex
        && old.get("tokens").is_none_or(Value::is_null)
        && old.get("OPENAI_API_KEY").and_then(Value::as_str).is_some()
    {
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
    // The request runs without the store lock (the refresh and identity
    // locks still exclude every other refresher of this login), so no
    // supervisor's terminal waits on the network.
    drop(store_lock.take());
    let result = (|| {
        let body = request_body(account.provider, &old)?;
        let response = request(account.provider, &body)?;
        apply_response(account.provider, &old, &response, at)
    })();
    let _store_lock = if store::lock_held() {
        None
    } else {
        Some(
            store
                .lock()
                .map_err(|_| Failure::new("usage-unavailable"))?,
        )
    };
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
    write_pending(store, account, replaces, &old, &auth);
    let aliases = aliases(store, account).map_err(|_| Failure::new("usage-unavailable"))?;
    // A label could have been added while the request was in flight.
    require_clear_aliases(store, &aliases).map_err(|_| Failure::new("usage-unavailable"))?;
    install_login(store, account, &aliases, &auth)
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
/// `<auth file>.pending` beside the login: private by name
/// (shared::private), never linked into a view.
fn pending_path(path: &std::path::Path) -> std::path::PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_owned();
    name.push(".pending");
    path.with_file_name(name)
}
fn configured_runtime_root() -> Option<std::path::PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .filter(|dir| dir.is_absolute())
}
fn runtime_pending_dir() -> Option<std::path::PathBuf> {
    configured_runtime_root()
        .filter(|dir| dir.is_dir())
        .map(|dir| dir.join("aswap"))
}
fn runtime_pending_prefix(store: &Store) -> String {
    bytes_hash(store.root.as_os_str().as_encoded_bytes())[..16].to_owned()
}
/// A second pending copy in `$XDG_RUNTIME_DIR` (a tmpfs on systemd hosts),
/// so a full or failing data disk can't lose both. Named per store, so
/// two stores (a test's and the real one) never share it.
fn runtime_pending_path(store: &Store, account: &Account) -> Option<std::path::PathBuf> {
    Some(runtime_pending_dir()?.join(format!(
        "{}-{}-{}.pending.json",
        runtime_pending_prefix(store),
        account.provider,
        account.label
    )))
}
pub(crate) fn pending_paths(store: &Store, account: &Account) -> Vec<std::path::PathBuf> {
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
    // Lets recovery finish an interrupted alias install after the origin
    // already holds the rotation. Older sidecars lack this fingerprint.
    base_pair: Option<String>,
    auth: Value,
}
fn file_hash(path: &std::path::Path) -> Option<String> {
    let bytes = store::read_private(path).ok()?;
    Some(bytes_hash(&bytes))
}
pub(crate) fn bytes_hash(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
/// Write both pending copies; either one is enough. `replaces` is the hash
/// of the login file the rotation started from.
fn write_pending(
    store: &Store,
    account: &Account,
    replaces: Option<String>,
    old: &Value,
    auth: &Value,
) {
    let Some(replaces) = replaces else {
        return;
    };
    let pending = Pending {
        replaces,
        base_pair: provider::credential_pair(account.provider, old),
        auth: auth.clone(),
    };
    for copy in pending_paths(store, account) {
        let _ = store::json_write(&copy, &pending);
    }
}

/// A directory can block a sidecar write but contains no pending login.
fn pending_copy(path: &std::path::Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => !metadata.is_dir(),
        Err(error) => error.kind() != std::io::ErrorKind::NotFound,
    }
}
pub(crate) fn pending_exists(store: &Store, account: &Account) -> bool {
    pending_paths(store, account)
        .iter()
        .any(|path| pending_copy(path))
}
/// Check the legacy store before admitting a mixed-version process. Check
/// names and metadata only, including leftover sidecars whose label is no
/// longer in state.json. Old releases cannot finish such a rotation.
pub(crate) fn require_legacy_pending_free(store: &Store) -> crate::Result<()> {
    use std::io::ErrorKind::NotFound;
    for provider in [Provider::Claude, Provider::Codex] {
        let dir = store.root.join("profiles").join(provider.as_str());
        store::no_links(&dir)?;
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == NotFound => continue,
            Err(_) => return Err(LEGACY_PENDING_INSPECT),
        };
        for entry in entries {
            let profile = entry.map_err(|_| LEGACY_PENDING_INSPECT)?.path();
            if !std::fs::symlink_metadata(&profile)
                .map_err(|_| LEGACY_PENDING_INSPECT)?
                .is_dir()
            {
                continue;
            }
            match std::fs::symlink_metadata(pending_path(&profile.join(provider.auth_file()))) {
                Ok(_) => return Err(LEGACY_PENDING),
                Err(err) if err.kind() == NotFound => {}
                Err(_) => return Err(LEGACY_PENDING_INSPECT),
            }
        }
    }
    let prefix = runtime_pending_prefix(store);
    let configured = configured_runtime_root();
    #[cfg(target_os = "linux")]
    let configured = {
        // An SSH shell may omit XDG_RUNTIME_DIR while a service under the
        // same UID has written its sole copy into the standard runtime.
        // SAFETY: geteuid takes no arguments.
        let uid = unsafe { libc::geteuid() };
        let standard = std::path::Path::new("/run/user").join(uid.to_string());
        require_runtime_pending_free(&standard, &prefix, Some(uid))?;
        configured.filter(|dir| dir != &standard)
    };
    if let Some(dir) = configured {
        require_runtime_pending_free(&dir, &prefix, None)?;
    }
    Ok(())
}
const LEGACY_PENDING: crate::Error = crate::Error(
    "legacy compatibility requires a store with no pending logins; resolve them before sharing the store with an older aswap",
);
const LEGACY_PENDING_INSPECT: crate::Error =
    crate::Error("cannot inspect legacy store for pending logins");

fn require_runtime_pending_free(
    runtime: &std::path::Path,
    prefix: &str,
    required_owner: Option<u32>,
) -> crate::Result<()> {
    use std::{io::ErrorKind::NotFound, os::unix::fs::MetadataExt};

    let dir = runtime.join("aswap");
    store::no_links(&dir)?;
    if let Some(uid) = required_owner {
        for path in [runtime, &dir] {
            match std::fs::symlink_metadata(path) {
                Ok(metadata) if metadata.is_dir() && metadata.uid() == uid => {}
                Err(err) if err.kind() == NotFound => return Ok(()),
                _ => return Err(LEGACY_PENDING_INSPECT),
            }
        }
    }
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == NotFound => return Ok(()),
        Err(_) => return Err(LEGACY_PENDING_INSPECT),
    };
    let claude = format!("{prefix}-claude-");
    let codex = format!("{prefix}-codex-");
    for entry in entries {
        let name = entry.map_err(|_| LEGACY_PENDING_INSPECT)?.file_name();
        if let Some(name) = name.to_str()
            && name.ends_with(".pending.json")
            && (name.starts_with(&claude) || name.starts_with(&codex))
        {
            return Err(LEGACY_PENDING);
        }
    }
    Ok(())
}
pub(crate) fn discard_pending(store: &Store, account: &Account) {
    for copy in pending_paths(store, account) {
        let _ = std::fs::remove_file(copy);
    }
}
/// All labels that an install will rewrite. The same identity fingerprint
/// selects these aliases and the refresh lock protecting them.
pub(crate) fn aliases(store: &Store, account: &Account) -> crate::Result<Vec<Account>> {
    Ok(
        crate::identity::duplicates(store, account, &store.load()?.accounts)
            .into_iter()
            .map(|label| Account {
                provider: account.provider,
                label,
            })
            .collect(),
    )
}
pub(crate) fn require_clear_aliases(store: &Store, aliases: &[Account]) -> crate::Result<()> {
    if aliases.iter().any(|alias| pending_exists(store, alias)) {
        return Err(crate::Error(
            "an alias has an unresolved pending login; kept",
        ));
    }
    Ok(())
}
fn login_path(store: &Store, account: &Account) -> crate::Result<std::path::PathBuf> {
    Ok(store
        .profile(account.provider, &account.label)?
        .join(account.provider.auth_file()))
}
/// Caller holds the store and identity refresh locks and has checked pending
/// copies for every alias it passes in.
pub(crate) fn install_login(
    store: &Store,
    account: &Account,
    aliases: &[Account],
    auth: &Value,
) -> crate::Result<()> {
    store::json_write(&login_path(store, account)?, auth)?;
    // The old access token may already be revoked. Update running views now.
    crate::shared::hand_out(store, account, auth);
    propagate(store, aliases, auth)?;
    crate::usage::credentials_changed(store, account)?;
    discard_pending(store, account);
    Ok(())
}
/// Install pending auth under the refresh lock before a rename.
pub(crate) fn settle_pending(store: &Store, account: &Account) -> crate::Result<()> {
    if store.legacy_compatibility() {
        return require_legacy_pending_free(store);
    }
    // Under the store lock: only ever try the refresh lock (lock order).
    let _refresh = store.try_refresh_lock(account)?.ok_or(crate::Error(BUSY))?;
    recover_pending(store, account)
}
/// What a store-lock holder gets when an account's refresh lock is taken
/// (a token refresh is running): drop the store lock and try again.
pub const BUSY: &str = "a token refresh is in progress; retry";

/// Inspect all copies before writing any alias. A copy whose base no longer
/// exists is stale, except when its login is already in the origin profile
/// and an interrupted install may still need to reach the other aliases.
pub(crate) fn recover_pending(store: &Store, account: &Account) -> crate::Result<()> {
    let aliases = aliases(store, account)?;
    let group = std::iter::once(account).chain(&aliases);
    let mut selected: Option<(&Account, Pending, Option<String>)> = None;
    let mut copies = Vec::new();
    for member in group.clone() {
        let member_copies: Vec<_> = pending_paths(store, member)
            .into_iter()
            .filter(|path| pending_copy(path))
            .collect();
        if member_copies.is_empty() {
            continue;
        }
        let bytes = store::read_private(&login_path(store, member)?)?;
        let current: Value = serde_json::from_slice(&bytes)
            .map_err(|_| crate::Error("cannot read the login to recover its pending copy"))?;
        let current_hash = bytes_hash(&bytes);
        for copy in member_copies {
            let pending: Pending = store::json_read(&copy)
                .map_err(|_| crate::Error("a pending login can't be read; kept"))?;
            copies.push(copy);
            let base = if pending.replaces == current_hash {
                provider::credential_pair(member.provider, &current)
            } else if pending.auth == current {
                pending.base_pair.clone()
            } else {
                continue;
            };
            provider::validate_auth(
                member.provider,
                &serde_json::to_vec(&pending.auth)
                    .map_err(|_| crate::Error("cannot encode pending login"))?,
            )?;
            if let Some((_, chosen, _)) = &selected {
                if chosen.auth != pending.auth {
                    return Err(crate::Error("conflicting pending logins; kept"));
                }
            } else {
                selected = Some((member, pending, base));
            }
        }
    }
    if let Some((origin, pending, base)) = selected {
        let rotated = provider::credential_pair(origin.provider, &pending.auth);
        let others: Vec<_> = group.filter(|m| m.label != origin.label).cloned().collect();
        for other in &others {
            let saved = store::json_read(&login_path(store, other)?)?;
            let pair = provider::credential_pair(other.provider, &saved);
            if pair != base && pair != rotated {
                return Err(crate::Error(
                    "an alias login differs from the pending rotation's base; kept",
                ));
            }
        }
        install_login(store, origin, &others, &pending.auth)?;
    }
    for copy in copies {
        let _ = std::fs::remove_file(copy);
    }
    Ok(())
}
/// An active local run may refresh even when inactive-profile monitoring is off.
/// Followers still only pull from the designated owner.
pub fn for_run(store: &Store, account: &Account) -> std::result::Result<bool, Failure> {
    for_run_with(store, account, crate::now(), request)
}
/// `for_run` with the token request given (tests).
pub fn for_run_with<F>(
    store: &Store,
    account: &Account,
    at: u64,
    request: F,
) -> std::result::Result<bool, Failure>
where
    F: FnOnce(Provider, &Value) -> std::result::Result<Value, Failure>,
{
    // Every supervisor asks on its terminal thread every 30 s. Answer "not
    // due" without the store lock, so a slow lock holder never stalls
    // typing; refresh_inner decides again under the lock when it may be due.
    if !may_be_due(store, account, at) {
        return Ok(false);
    }
    refresh_inner(store, account, at, false, true, request)
}
/// False only when an unforced refresh_inner would certainly do nothing:
/// the stored login (written atomically, so safe to read unlocked) has a
/// known expiry beyond the margin, or none at all.
fn may_be_due(store: &Store, account: &Account, at: u64) -> bool {
    if account.provider == Provider::Agy {
        return false;
    }
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
    // agy keeps its own login fresh in the view, so it gets the whole login;
    // a newer one is written back to the profile (View::sync).
    if provider == Provider::Agy {
        return auth.clone();
    }
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
fn propagate(store: &Store, aliases: &[Account], auth: &Value) -> crate::Result<()> {
    for other in aliases {
        let path = login_path(store, other)?;
        let mut saved: Value = store::json_read(&path)?;
        let field = if other.provider == Provider::Claude {
            "claudeAiOauth"
        } else {
            "tokens"
        };
        if let Some(value) = auth.get(field) {
            saved[field] = value.clone();
        }
        if other.provider == Provider::Codex
            && let Some(value) = auth.get("last_refresh")
        {
            saved["last_refresh"] = value.clone();
        }
        store::json_write(&path, &saved)?;
        crate::shared::hand_out(store, other, &saved);
        crate::usage::credentials_changed(store, other)?;
    }
    Ok(())
}
#[cfg(test)]
mod pending_tests {
    use super::*;

    #[test]
    fn owned_runtime_preflight_finds_a_sole_sidecar_by_name() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path().join("store")).unwrap();
        let runtime = temp.path().join("run/user/synthetic");
        std::fs::create_dir_all(&runtime).unwrap();
        let uid = std::fs::metadata(&runtime).unwrap().uid();
        let prefix = runtime_pending_prefix(&store);
        let dir = runtime.join("aswap");

        require_runtime_pending_free(&runtime, &prefix, Some(uid)).unwrap();
        assert!(!dir.exists());

        std::fs::create_dir(&dir).unwrap();
        std::fs::write(
            dir.join("different-store-codex-orphan.pending.json"),
            b"synthetic",
        )
        .unwrap();
        std::fs::write(
            dir.join(format!("{prefix}-agy-orphan.pending.json")),
            b"synthetic",
        )
        .unwrap();
        require_runtime_pending_free(&runtime, &prefix, Some(uid)).unwrap();
        assert_eq!(
            require_runtime_pending_free(&runtime, &prefix, Some(uid.wrapping_add(1)))
                .unwrap_err()
                .0,
            LEGACY_PENDING_INSPECT.0
        );

        for provider in [Provider::Claude, Provider::Codex] {
            let pending = dir.join(format!("{prefix}-{provider}-orphan.pending.json"));
            std::fs::write(
                &pending,
                b"this synthetic sidecar is unreadable and invalid",
            )
            .unwrap();
            std::fs::set_permissions(&pending, std::fs::Permissions::from_mode(0o0)).unwrap();
            assert_eq!(
                require_runtime_pending_free(&runtime, &prefix, Some(uid))
                    .unwrap_err()
                    .0,
                LEGACY_PENDING.0
            );
            assert!(pending.is_file());
            std::fs::remove_file(pending).unwrap();
        }
    }

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
                base_pair: None,
                auth: serde_json::json!({"claudeAiOauth":{"accessToken":"synthetic"}}),
            },
        )
        .unwrap();
        assert!(recover_pending(&store, &account).is_err());
        assert!(copy.exists(), "the pending copy was dropped");
        // Once the file reads and differs, the copy is stale and dropped.
        std::fs::remove_dir(&path).unwrap();
        std::fs::write(&path, b"{}").unwrap();
        recover_pending(&store, &account).unwrap();
        assert!(!copy.exists());
    }
}
