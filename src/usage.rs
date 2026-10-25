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
pub struct Cached {
    pub usage: Usage,
    pub fetched_at: u64,
    pub next_poll_at: u64,
    pub failures: u32,
    pub error: Option<String>,
}
impl Cached {
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
    pub kind: &'static str,
    pub retry_after: u64,
}
impl Failure {
    fn unavailable() -> Self {
        Self {
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
        return codex_rpc(profile)
            .and_then(|v| parse_codex(&v))
            .map_err(|_| Failure::unavailable());
    }
    let auth: Value = store::json_read(&profile.join(account.provider.auth_file()))
        .map_err(|_| Failure::unavailable())?;
    let token = auth
        .pointer("/claudeAiOauth/accessToken")
        .and_then(Value::as_str)
        .ok_or(Failure::unavailable())?;
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
        .map_err(|_| Failure::unavailable())?;
    if !response.status().is_success() {
        return Err(Failure {
            kind: if response.status().as_u16() == 429 {
                "http-429"
            } else {
                "usage-http-error"
            },
            retry_after: response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse().ok())
                .unwrap_or(0),
        });
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
    parse_claude(&v).map_err(|_| Failure::unavailable())
}
pub fn updated(old: Cached, result: std::result::Result<Usage, Failure>, at: u64) -> Cached {
    match result {
        Ok(usage) => Cached {
            usage,
            fetched_at: at,
            next_poll_at: at.saturating_add(TTL),
            failures: 0,
            error: None,
        },
        Err(e) => {
            let failures = old.failures.saturating_add(1);
            let backoff = 300u64
                .saturating_mul(1u64 << failures.min(5))
                .min(3600)
                .max(e.retry_after.saturating_add(60));
            Cached {
                next_poll_at: at.saturating_add(backoff),
                failures,
                error: Some(e.kind.to_owned()),
                ..old
            }
        }
    }
}
pub fn cached_with<F>(store: &Store, account: &Account, at: u64, fetcher: F) -> Result<Cached>
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
    let old: Cached = if path.exists() {
        store::json_read(&path)?
    } else {
        Cached::default()
    };
    if at < old.next_poll_at {
        return Ok(old);
    }
    let entry = updated(old, fetcher(&profile), at);
    store::json_write(&path, &entry)?;
    Ok(entry)
}
pub fn cached(store: &Store, account: &Account) -> Result<Cached> {
    cached_with(store, account, now(), |p| fetch(account, p))
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
