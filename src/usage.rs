use crate::{
    Error, Result, Safe, now,
    provider::Provider,
    store::{self, Account, Store},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    io::{BufRead, BufReader, Read, Write},
    path::Path,
    process::{Child, Stdio},
    sync::mpsc,
    time::Duration,
};

pub const TTL: u64 = 180;
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Window {
    pub name: String,
    pub used_percent: f64,
    pub resets_at: Option<u64>,
    pub duration_minutes: Option<u64>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Usage {
    pub windows: Vec<Window>,
}
impl Usage {
    pub fn used(&self, at: u64) -> Option<f64> {
        // A reset makes observations obsolete, not evidence of an empty account.
        if self
            .windows
            .iter()
            .any(|w| w.resets_at.is_some_and(|r| r <= at))
        {
            return None;
        }
        self.windows.iter().map(|w| w.used_percent).reduce(f64::max)
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Cached {
    pub http_status: Option<u16>,
    pub retry_after: Option<u64>,
    pub last_attempt: u64,
    pub usage: Usage,
    pub fetched_at: u64,
    pub next_poll_at: u64,
    pub failures: u32,
    pub error: Option<String>,
}
impl Cached {
    pub fn diagnostic(&self) -> Option<serde_json::Value> {
        let error = self.error.as_deref()?;
        let class = match error {
            "http-401" | "refresh-http-401" => "expired-or-invalid-token",
            "http-403" | "refresh-http-403" => "forbidden",
            "http-429" | "refresh-http-429" => "rate-limited",
            "network" => "network",
            "codex-rpc-unavailable" => "codex-rpc-unavailable",
            "codex-rpc-start-failed" => "codex-rpc-start-failed",
            "codex-rpc-timeout" => "codex-rpc-timeout",
            "codex-rpc-error" => "codex-rpc-error",
            "codex-rpc-unsupported" => "codex-rpc-unsupported",
            "provider-limit" => "provider-limit",
            "refresh-invalid-grant" => "refresh-invalid-grant",
            "refresh-owner-required" => "refresh-owner-required",
            "refresh-invalid-response" => "refresh-invalid-response",
            "sync-unavailable" => "sync-unavailable",
            "owner-profile-missing" => "owner-profile-missing",
            "http-error" | "usage-http-error" | "refresh-http-error" => "http-error",
            _ => "usage-unavailable",
        };
        let status = self
            .http_status
            .filter(|s| (100..600).contains(s))
            .or(match error {
                "http-401" => Some(401),
                "http-403" => Some(403),
                "http-429" => Some(429),
                _ => None,
            });
        Some(
            serde_json::json!({"class":class,"http_status":status,"retry_after_seconds":self.retry_after,"source":if error.starts_with("refresh-"){"oauth-refresh"}else if error.starts_with("codex-"){"codex-app-server"}else{"usage"}}),
        )
    }
    pub fn used(&self, at: u64) -> Option<f64> {
        if self.error.is_some() || at.saturating_sub(self.fetched_at) > 600 {
            None
        } else {
            self.usage.used(at)
        }
    }
}
fn percent(value: Option<&Value>) -> Result<f64> {
    let p = value
        .and_then(Value::as_f64)
        .ok_or(Error("invalid usage percentage; response redacted"))?;
    if !p.is_finite() || !(0.0..=100.0).contains(&p) {
        return Err(Error("usage percentage outside valid range"));
    }
    Ok(p)
}
pub fn parse_claude(v: &Value) -> Result<Usage> {
    let mut windows = Vec::new();
    for key in [
        "five_hour",
        "seven_day",
        "seven_day_opus",
        "seven_day_sonnet",
    ] {
        if let Some(w) = v.get(key).filter(|w| !w.is_null()) {
            let resets_at = w
                .get("resets_at")
                .and_then(Value::as_str)
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .and_then(|d| u64::try_from(d.timestamp()).ok());
            windows.push(Window {
                name: key.to_owned(),
                used_percent: percent(w.get("utilization"))?,
                resets_at,
                duration_minutes: Some(if key == "five_hour" { 300 } else { 10080 }),
            });
        }
    }
    if windows.is_empty() {
        return Err(Error("no supported Claude usage windows"));
    }
    Ok(Usage { windows })
}
pub fn parse_codex(v: &Value) -> Result<Usage> {
    let mut windows = Vec::new();
    // The standard codex bucket is comparable across profiles. Do not mix
    // unrelated model buckets into the account-selection metric.
    let bucket = v
        .pointer("/rateLimitsByLimitId/codex")
        .or_else(|| v.get("rateLimits"));
    if let Some(bucket) = bucket {
        for key in ["primary", "secondary"] {
            if let Some(w) = bucket.get(key).filter(|w| !w.is_null()) {
                windows.push(Window {
                    name: key.to_owned(),
                    used_percent: percent(w.get("usedPercent"))?,
                    resets_at: w.get("resetsAt").and_then(Value::as_u64),
                    duration_minutes: w.get("windowDurationMins").and_then(Value::as_u64),
                });
            }
        }
    }
    Ok(Usage { windows })
}
#[derive(Clone, Copy, Debug)]
pub struct Failure {
    pub status: Option<u16>,
    pub kind: &'static str,
    pub retry_after: u64,
}
impl Failure {
    pub fn new(kind: &'static str) -> Self {
        Self {
            status: None,
            kind,
            retry_after: 0,
        }
    }
    pub fn http(status: u16, retry_after: u64) -> Self {
        Self {
            status: Some(status),
            retry_after,
            kind: match status {
                401 => "http-401",
                403 => "http-403",
                429 => "http-429",
                _ => "http-error",
            },
        }
    }
    fn unavailable() -> Self {
        Self {
            status: None,
            kind: "usage-unavailable",
            retry_after: 0,
        }
    }
}
struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
/// Bounded stdio JSON-RPC client. Child stderr and RPC errors are never exposed.
pub fn codex_rpc(profile: &Path) -> Result<Value> {
    codex_rpc_inner(
        profile,
        &std::cell::Cell::new(Failure::new("codex-rpc-unavailable")),
    )
}
fn codex_rpc_inner(profile: &Path, diagnostic: &std::cell::Cell<Failure>) -> Result<Value> {
    let mut child = Process(
        Provider::Codex
            .command(profile)
            .args(["app-server", "--listen", "stdio://"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .safe("cannot start Codex app-server")?,
    );
    let mut input = child
        .0
        .stdin
        .take()
        .ok_or(Error("missing app-server input"))?;
    let output = child
        .0
        .stdout
        .take()
        .ok_or(Error("missing app-server output"))?;
    let (tx, rx) = mpsc::sync_channel(32);
    let reader = std::thread::spawn(move || {
        let mut reader = BufReader::new(output);
        loop {
            let mut line = Vec::new();
            match reader
                .by_ref()
                .take(1024 * 1024 + 1)
                .read_until(b'\n', &mut line)
            {
                Ok(0) | Err(_) => break,
                Ok(_) if line.len() > 1024 * 1024 => break,
                _ => {}
            }
            if let Ok(v) = serde_json::from_slice::<Value>(&line)
                && tx.try_send(v).is_err()
            {
                break;
            }
        }
    });
    let result = (|| {
        let send = |input: &mut std::process::ChildStdin, v: Value| -> Result<()> {
            writeln!(input, "{v}").safe("app-server write failed")?;
            input.flush().safe("app-server flush failed")
        };
        let receive = |id: u64| -> Result<Value> {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            for _ in 0..128 {
                let remain = deadline.saturating_duration_since(std::time::Instant::now());
                let v = rx
                    .recv_timeout(remain)
                    .safe("app-server response timed out or disconnected")?;
                if v.get("id").and_then(Value::as_u64) == Some(id) {
                    if v.get("error").is_some() {
                        let error = &v["error"];
                        let status = [
                            "/data/http_status",
                            "/data/httpStatus",
                            "/data/status",
                            "/data/statusCode",
                        ]
                        .iter()
                        .find_map(|p| {
                            error
                                .pointer(p)
                                .and_then(Value::as_u64)
                                .and_then(|n| u16::try_from(n).ok())
                        });
                        let status = status.or_else(|| {
                            let message =
                                error.get("message").and_then(Value::as_str).unwrap_or("");
                            [
                                (401, ["HTTP 401", "401 Unauthorized", "status code: 401"]),
                                (403, ["HTTP 403", "403 Forbidden", "status code: 403"]),
                                (
                                    429,
                                    ["HTTP 429", "429 Too Many Requests", "status code: 429"],
                                ),
                            ]
                            .into_iter()
                            .find_map(|(code, markers)| {
                                markers.iter().any(|m| message.contains(m)).then_some(code)
                            })
                        });
                        let retry = error
                            .pointer("/data/retryAfter")
                            .and_then(Value::as_u64)
                            .unwrap_or(0);
                        diagnostic.set(if let Some(status) = status {
                            Failure::http(status, retry)
                        } else {
                            Failure::new("codex-rpc-error")
                        });
                    }
                    return v
                        .get("result")
                        .cloned()
                        .ok_or(Error("app-server request failed; details redacted"));
                }
            }
            Err(Error("too many app-server notifications"))
        };
        send(
            &mut input,
            serde_json::json!({"id":1,"method":"initialize","params":{"clientInfo":{"name":"aswap","version":env!("CARGO_PKG_VERSION")}}}),
        )?;
        receive(1)?;
        send(
            &mut input,
            serde_json::json!({"method":"initialized","params":{}}),
        )?;
        send(
            &mut input,
            serde_json::json!({"id":2,"method":"account/rateLimits/read"}),
        )?;
        receive(2)
    })();
    drop(input);
    drop(child);
    let _ = reader.join();
    result
}
pub fn fetch(account: &Account, profile: &Path) -> std::result::Result<Usage, Failure> {
    if account.provider == Provider::Codex {
        let diagnostic = std::cell::Cell::new(Failure::new("codex-rpc-unavailable"));
        let result = (|| -> Result<Value> {
            // The usage-only app-server cannot rotate auth behind a running CLI.
            let temporary = tempfile::tempdir().safe("cannot prepare usage auth")?;
            let auth: Value = store::json_read(&profile.join(account.provider.auth_file()))?;
            store::json_write(
                &temporary.path().join(account.provider.auth_file()),
                &crate::refresh::access_only(account.provider, &auth),
            )?;
            codex_rpc_inner(temporary.path(), &diagnostic)
        })();
        return result.and_then(|v| parse_codex(&v)).map_err(|e| {
            let mut failure = diagnostic.get();
            if e.0 == "cannot start Codex app-server" {
                failure.kind = "codex-rpc-start-failed";
            } else if e.0 == "app-server response timed out or disconnected" {
                failure.kind = "codex-rpc-timeout";
            }
            failure
        });
    }
    fetch_claude_with(profile, request_claude)
}
fn fetch_claude_with<F>(profile: &Path, request: F) -> std::result::Result<Usage, Failure>
where
    F: FnOnce(&str) -> std::result::Result<Value, Failure>,
{
    let auth: Value = store::json_read(&profile.join(Provider::Claude.auth_file()))
        .map_err(|_| Failure::unavailable())?;
    let token = auth
        .pointer("/claudeAiOauth/accessToken")
        .and_then(Value::as_str)
        .ok_or(Failure::unavailable())?;
    parse_claude(&request(token)?).map_err(|_| Failure::unavailable())
}
fn request_claude(token: &str) -> std::result::Result<Value, Failure> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| Failure::unavailable())?;
    let response = client
        .get("https://api.anthropic.com/api/oauth/usage")
        .bearer_auth(token)
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("User-Agent", "aswap/0.1")
        .send()
        .map_err(|_| Failure::new("network"))?;
    if !response.status().is_success() {
        let retry_after = parse_retry_after(
            response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok()),
            now(),
        );
        return Err(Failure::http(response.status().as_u16(), retry_after));
    }
    let mut body = Vec::new();
    use std::io::Read;
    response
        .take(1024 * 1024 + 1)
        .read_to_end(&mut body)
        .map_err(|_| Failure::unavailable())?;
    if body.len() > 1024 * 1024 {
        return Err(Failure::unavailable());
    }
    let v: Value = serde_json::from_slice(&body).map_err(|_| Failure::unavailable())?;
    Ok(v)
}
pub fn parse_retry_after(header: Option<&str>, at: u64) -> u64 {
    header
        .and_then(|s| {
            s.parse::<u64>().ok().or_else(|| {
                chrono::DateTime::parse_from_rfc2822(s)
                    .ok()
                    .map(|d| (d.timestamp().max(0) as u64).saturating_sub(at))
            })
        })
        .unwrap_or(0)
}
pub fn updated(old: Cached, result: std::result::Result<Usage, Failure>, at: u64) -> Cached {
    match result {
        Ok(usage) => Cached {
            usage,
            fetched_at: at,
            next_poll_at: at.saturating_add(TTL),
            failures: 0,
            error: None,
            last_attempt: at,
            http_status: None,
            retry_after: None,
        },
        Err(e) => {
            let failures = old.failures.saturating_add(1);
            let backoff = 60u64
                .saturating_mul(1u64 << failures.saturating_sub(1).min(4))
                .min(900)
                .max(e.retry_after);
            Cached {
                next_poll_at: at.saturating_add(backoff),
                failures,
                error: Some(e.kind.to_owned()),
                last_attempt: at,
                http_status: e.status,
                retry_after: (e.retry_after > 0).then_some(e.retry_after),
                ..old
            }
        }
    }
}
pub fn cached_with<F>(store: &Store, account: &Account, at: u64, fetcher: F) -> Result<Cached>
where
    F: FnOnce(&Path) -> std::result::Result<Usage, Failure>,
{
    cached_mode_with(store, account, at, false, fetcher)
}
pub fn cached_mode_with<F>(
    store: &Store,
    account: &Account,
    at: u64,
    force: bool,
    fetcher: F,
) -> Result<Cached>
where
    F: FnOnce(&Path) -> std::result::Result<Usage, Failure>,
{
    // A cross-process store lock serializes the cache claim and fetch; timeout
    // bounds are deliberately short. This prevents CLI/Waybar/auto stampedes.
    let _lock = store.lock()?;
    let state = store.load()?;
    store.account(&state, account.provider, Some(&account.label))?;
    let profile = store.profile(account.provider, &account.label)?;
    let path = profile.join("usage-cache.json");
    let mut old: Cached = if path.exists() {
        store::json_read(&path)?
    } else {
        Cached::default()
    };
    if old.error.is_some() && old.retry_after.is_none() {
        old.next_poll_at = old
            .next_poll_at
            .min(old.last_attempt.max(old.fetched_at).saturating_add(900));
    }
    let reset_due = old.usage.windows.iter().any(|w| {
        w.resets_at
            .is_some_and(|r| r <= at && old.last_attempt.max(old.fetched_at) < r)
    });
    if !force && !reset_due && at < old.next_poll_at {
        return Ok(old);
    }
    let entry = updated(old, fetcher(&profile), at);
    store::json_write(&path, &entry)?;
    Ok(entry)
}
pub fn cached(store: &Store, account: &Account) -> Result<Cached> {
    cached_force(store, account, false)
}
pub fn cached_force(store: &Store, account: &Account, force: bool) -> Result<Cached> {
    if crate::coordination::follower(store)? {
        let result = crate::coordination::poll(store, force.then_some(account), false);
        let _lock = store.lock()?;
        let path = store
            .profile(account.provider, &account.label)?
            .join("usage-cache.json");
        let mut cached: Cached = if path.exists() {
            store::json_read(&path)?
        } else {
            Cached::default()
        };
        if result.is_err() {
            cached.error = Some("sync-unavailable".into());
            cached.http_status = None;
        }
        return Ok(cached);
    }

    cached_mode_with(store, account, now(), force, |p| {
        crate::refresh::refresh(store, account, now(), false)?;
        let first = fetch(account, p);
        if first
            .as_ref()
            .err()
            .is_some_and(|e| e.status == Some(401) || e.kind == "codex-rpc-unavailable")
            && crate::refresh::refresh(store, account, now(), true)?
        {
            fetch(account, p)
        } else {
            first
        }
    })
}
/// Most headroom, only fresh known quotas, ten-point hysteresis and cooldown.
pub fn choose<'a>(
    current: &str,
    entries: &'a [(Account, Cached)],
    threshold: f64,
    last_switch: u64,
    at: u64,
    limited: bool,
) -> Option<&'a Account> {
    let used = entries.iter().find(|(a, _)| a.label == current)?.1.used(at);
    if !limited
        && used.is_none_or(|v| v < threshold || (v < 100.0 && at.saturating_sub(last_switch) < 300))
    {
        return None;
    }
    entries
        .iter()
        .filter(|(a, c)| {
            a.label != current
                && c.used(at).is_some_and(|v| {
                    v < threshold && (limited || used.is_some_and(|u| u - v >= 10.0))
                })
        })
        .min_by(|(_, a), (_, b)| a.used(at).unwrap().total_cmp(&b.used(at).unwrap()))
        .map(|(a, _)| a)
}

pub fn mark_limited(store: &Store, account: &Account) -> Result<()> {
    let _lock = store.lock()?;
    let path = store
        .profile(account.provider, &account.label)?
        .join("usage-cache.json");
    let mut entry: Cached = if path.exists() {
        store::json_read(&path)?
    } else {
        Cached::default()
    };
    entry.error = Some("provider-limit".into());
    if entry.http_status != Some(429) {
        entry.http_status = None;
        entry.retry_after = None;
    }
    entry.next_poll_at = entry.next_poll_at.max(now().saturating_add(TTL));
    store::json_write(&path, &entry)
}

/// Caller holds the store lock. A newly rotated access token invalidates only
/// authentication backoff; a provider's Retry-After remains authoritative.
pub fn credentials_changed(store: &Store, account: &Account) -> Result<()> {
    let path = store
        .profile(account.provider, &account.label)?
        .join("usage-cache.json");
    if path.exists() {
        let mut cache: Cached = store::json_read(&path)?;
        if cache.http_status == Some(401)
            || cache.error.as_deref().is_some_and(|s| {
                matches!(s, "http-401" | "refresh-invalid-grant" | "refresh-http-401")
            })
        {
            cache.next_poll_at = now();
            store::json_write(&path, &cache)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod isolation_tests {
    use super::*;
    #[test]
    fn profile_credentials_and_caches_are_isolated() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path().join("store")).unwrap();
        let accounts: Vec<_> = ["one", "two"]
            .map(|label| Account {
                provider: Provider::Claude,
                label: label.into(),
            })
            .into();
        let mut state = store.load().unwrap();
        state.accounts = accounts.clone();
        state.active.insert(Provider::Claude, "one".into());
        store.save(&state).unwrap();
        for (index, account) in accounts.iter().enumerate() {
            let profile = store.profile(account.provider, &account.label).unwrap();
            store::json_write(&profile.join(".credentials.json"), &serde_json::json!({"claudeAiOauth":{"accessToken":format!("synthetic-token-{index}")}})).unwrap();
            let cached = cached_with(&store, account, 1000, |path| {
                assert_eq!(path, profile);
                fetch_claude_with(path, |token| {
                    assert_eq!(token, format!("synthetic-token-{index}"));
                    Ok(serde_json::json!({"five_hour":{"utilization":10.0 + index as f64 * 30.0}, "seven_day":{"utilization":99}}))
                })
            }).unwrap();
            assert_eq!(
                cached.usage.windows[0].used_percent,
                10.0 + index as f64 * 30.0
            );
            assert_eq!(cached.used(1000), Some(99.0));
        }
        for (index, account) in accounts.iter().enumerate() {
            let cached = cached_with(&store, account, 1001, |_| {
                panic!("must use this profile's cache")
            })
            .unwrap();
            assert_eq!(
                cached.usage.windows[0].used_percent,
                10.0 + index as f64 * 30.0
            );
        }
    }
}
