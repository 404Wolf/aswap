use crate::{
    Error, Result, Safe, now,
    provider::Provider,
    store::{self, Account, Prefer, Store},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
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
    /// Codex slot index, used to identify windows previously seen without a duration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_slot: Option<u8>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Usage {
    #[serde(deserialize_with = "deserialize_windows")]
    pub windows: Vec<Window>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credits: Option<Credits>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available_usage_resets: Option<u64>,
    /// Codex usage-limit reset credits as the app-server lists them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reset_credits: Vec<ResetCredit>,
    /// Whether the read listed credit details (`credits` was an array). When
    /// only the count is known (`credits` null), or the cache predates this
    /// field, availability stays unknown rather than "none".
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reset_credits_listed: bool,
    /// Codex's `ordinaryUsageAllowed`, the backend's permission for ordinary
    /// usage (see [`OrdinaryUsage`]).
    #[serde(default, skip_serializing_if = "OrdinaryUsage::not_reported")]
    pub ordinary_usage: OrdinaryUsage,
}
/// Codex's `ordinaryUsageAllowed` (app-server protocol v2): the backend's
/// permission for ordinary included usage. Upstream: "Null means
/// unavailable; clients must not infer recovery from percentages or reset
/// times." Older Codex versions don't report it at all.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OrdinaryUsage {
    /// The read has no such field: an older Codex. Percentages decide.
    #[default]
    NotReported,
    /// Reported as null: permission unknown.
    Unknown,
    Allowed,
    Denied,
}
impl OrdinaryUsage {
    fn not_reported(&self) -> bool {
        *self == Self::NotReported
    }
    pub fn from_read(v: &Value) -> Self {
        match v.get("ordinaryUsageAllowed") {
            None => Self::NotReported,
            Some(Value::Bool(true)) => Self::Allowed,
            Some(Value::Bool(false)) => Self::Denied,
            Some(_) => Self::Unknown,
        }
    }
    /// Whether this reading may count as recovered usage: not denied, and
    /// not unknown on a Codex that reports the permission.
    pub fn permits_recovery(self) -> bool {
        matches!(self, Self::NotReported | Self::Allowed)
    }
}
/// One Codex usage-limit reset credit. Redeeming it resets the account's
/// eligible windows (5-hour and weekly together); it is an earned, expiring
/// allowance. Text fields are bounded and stripped of control characters.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResetCredit {
    /// Opaque backend identifier, passed back when redeeming.
    pub id: String,
    /// `available`, `redeeming`, `redeemed` or `unknown`.
    pub status: String,
    /// `codexRateLimits` or `unknown`.
    pub reset_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}
impl ResetCredit {
    pub fn available(&self) -> bool {
        self.status == "available" && self.reset_type == "codexRateLimits"
    }
}
const MAX_RESET_CREDITS: usize = 16;
fn bounded_text(value: Option<&Value>) -> Option<String> {
    let text: String = value?
        .as_str()?
        .chars()
        .filter(|c| !c.is_control())
        .take(200)
        .collect();
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}
fn parse_reset_credit(v: &Value) -> Option<ResetCredit> {
    let id = v.get("id")?.as_str()?;
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b))
    {
        return None;
    }
    let known = |key: &str, values: &[&str]| {
        let value = v.get(key).and_then(Value::as_str).unwrap_or("");
        if values.contains(&value) {
            value
        } else {
            "unknown"
        }
        .to_owned()
    };
    Some(ResetCredit {
        id: id.to_owned(),
        status: known("status", &["available", "redeeming", "redeemed"]),
        reset_type: known("resetType", &["codexRateLimits"]),
        expires_at: v.get("expiresAt").and_then(Value::as_u64),
        title: bounded_text(v.get("title")),
        description: bounded_text(v.get("description")),
    })
}
pub fn parse_reset_credits(v: &Value) -> Vec<ResetCredit> {
    v.pointer("/rateLimitResetCredits/credits")
        .and_then(Value::as_array)
        .map(|credits| {
            credits
                .iter()
                .take(MAX_RESET_CREDITS)
                .filter_map(parse_reset_credit)
                .collect()
        })
        .unwrap_or_default()
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Credits {
    pub balance: Option<String>,
    pub unlimited: Option<bool>,
    // The current RPC does not expose reload settings. Never infer from balance.
    pub automatic_reload: Option<bool>,
}
fn codex_window_name(duration: Option<u64>) -> String {
    match duration {
        Some(300) => "session".into(),
        Some(10080) => "weekly".into(),
        Some(n) => format!("window_{n}m"),
        None => "quota".into(),
    }
}
fn deserialize_windows<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Vec<Window>, D::Error> {
    let mut windows = Vec::<Window>::deserialize(deserializer)?;
    // Upgrade older caches before either JSON or terminal output can expose slot names.
    for w in &mut windows {
        if matches!(w.name.as_str(), "primary" | "secondary") {
            w.codex_slot = Some(u8::from(w.name == "secondary"));
            w.name = codex_window_name(w.duration_minutes);
        }
    }
    Ok(windows)
}
/// A reading older than this many seconds no longer drives selection.
pub const STALE_AFTER: u64 = 600;

/// Why a cached reading is or isn't usable (`usage_reason` in the JSON).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Reason {
    /// Usable: fresh, with windows, none past its reset.
    Fresh,
    /// No reading yet.
    NeverFetched,
    /// The last attempt failed; see `usage_error`.
    Error,
    /// A hard limit seen earlier is missing from the latest reading.
    UnverifiedLimit,
    /// Older than [`STALE_AFTER`] seconds.
    Stale,
    /// A window has passed its reset time since the reading.
    ResetPassed,
    /// The provider reported no windows.
    NoWindows,
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
        // Where Codex reports the backend's permission it decides: denied is
        // out of usage whatever the percentages say, and unknown (null) is
        // not a usable reading (upstream: "clients must not infer recovery
        // from percentages or reset times").
        match self.ordinary_usage {
            OrdinaryUsage::Denied => Some(100.0),
            OrdinaryUsage::Unknown => None,
            OrdinaryUsage::Allowed | OrdinaryUsage::NotReported => {
                self.windows.iter().map(|w| w.used_percent).reduce(f64::max)
            }
        }
    }
    /// When the account-wide weekly window resets; model-specific weekly
    /// windows only stand in when no account-wide one was observed.
    pub fn weekly_reset(&self, at: u64) -> Option<u64> {
        self.weekly(at).and_then(|w| w.resets_at)
    }
    /// The account-wide weekly window with a reset still ahead.
    pub fn weekly(&self, at: u64) -> Option<&Window> {
        let weekly =
            |w: &&Window| w.duration_minutes == Some(10080) && w.resets_at.is_some_and(|r| r > at);
        self.windows
            .iter()
            .filter(weekly)
            .find(|w| matches!(w.name.as_str(), "seven_day" | "weekly"))
            .or_else(|| {
                self.windows
                    .iter()
                    .filter(weekly)
                    .min_by_key(|w| w.resets_at)
            })
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
    /// Previously exhausted windows missing from the latest usable observation.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unverified_limits: Vec<UnverifiedLimit>,
    /// When the quota observation in `usage` was started, on the store's
    /// monotonic observation counter (`next_observation`). One real account
    /// is read by several programs: the later-started observation wins, even
    /// within one second, and an older fetch never overwrites a newer one.
    /// 0: written by a version without it (then `fetched_at` orders).
    #[serde(skip_serializing_if = "is_zero")]
    pub observation: u64,
}
fn is_zero(n: &u64) -> bool {
    *n == 0
}
impl Cached {
    /// Whether this quota observation is newer than `other`'s.
    pub(crate) fn newer_than(&self, other: &Self) -> bool {
        match (self.observation, other.observation) {
            (0, 0) => self.fetched_at > other.fetched_at,
            // A numbered observation beats a legacy one of the same second
            // (it can only have been started then or later); otherwise
            // time decides.
            (_, 0) => self.fetched_at >= other.fetched_at,
            (0, _) => self.fetched_at > other.fetched_at,
            (mine, theirs) => mine > theirs,
        }
    }
    /// This account's quota observation from `shared` (another login of the
    /// same account), keeping what belongs to this login: an auth failure, a
    /// server backoff, its own poll schedule. Only the account-level limit
    /// mark (`provider-limit`) gives way to a newer observation.
    pub(crate) fn adopt(&self, shared: &Self) -> Self {
        let mut next = self.clone();
        next.usage = shared.usage.clone();
        next.fetched_at = shared.fetched_at;
        next.observation = shared.observation;
        next.unverified_limits = shared.unverified_limits.clone();
        // Not due again before the observation it now holds would be; an
        // own backoff that runs later stays.
        next.next_poll_at = self.next_poll_at.max(shared.next_poll_at);
        next.preserve_limits(self);
        if self.error.as_deref() == Some("provider-limit") {
            next.error = None;
        }
        next
    }
    /// Whether this login's own state (an auth failure, a server backoff)
    /// keeps a shared observation from making it usable.
    fn own_failure(&self) -> bool {
        self.error.as_deref().is_some_and(|e| e != "provider-limit") || self.retry_after.is_some()
    }
}
/// The store's next quota observation number (see `Cached::observation`),
/// above `floor` too: an observation already present (synced in from
/// another store, whose counter this one doesn't have) never outranks a
/// fetch started now. Caller holds the store lock.
fn next_observation(store: &Store, floor: u64) -> Result<u64> {
    let path = store.root.join("usage-observations");
    let last: u64 = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    let next = last.max(now()).max(floor).saturating_add(1);
    store::atomic(&path, next.to_string().as_bytes())?;
    Ok(next)
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UnverifiedLimit {
    Observed { window: Window, observed_at: u64 },
    Legacy(Window),
}
impl UnverifiedLimit {
    fn window(&self) -> &Window {
        match self {
            Self::Observed { window, .. } | Self::Legacy(window) => window,
        }
    }

    fn observed_at(&self, fetched_at: u64) -> u64 {
        match self {
            Self::Observed { observed_at, .. } => *observed_at,
            // A flat limit predates this cache's latest successful poll. Use
            // that later timestamp as a conservative lower bound for clearing it.
            Self::Legacy(_) => fetched_at,
        }
    }
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
            "usage-unsupported" => "usage-unsupported",
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
        if self.error.is_some()
            || !self.unverified_limits.is_empty()
            || at.saturating_sub(self.fetched_at) > STALE_AFTER
        {
            None
        } else {
            self.usage.used(at)
        }
    }
    /// Why this reading is, or isn't, usable at `at`: exactly when it is
    /// [`Reason::Fresh`] does [`Cached::used`] return a value.
    pub fn reason(&self, at: u64) -> Reason {
        if self.error.is_some() {
            Reason::Error
        } else if self.fetched_at == 0 {
            Reason::NeverFetched
        } else if !self.unverified_limits.is_empty() {
            Reason::UnverifiedLimit
        } else if at.saturating_sub(self.fetched_at) > STALE_AFTER {
            Reason::Stale
        } else if self
            .usage
            .windows
            .iter()
            .any(|w| w.resets_at.is_some_and(|r| r <= at))
        {
            Reason::ResetPassed
        } else if self.usage.windows.is_empty() {
            Reason::NoWindows
        } else {
            Reason::Fresh
        }
    }
    /// Seconds since the reading was fetched; none if it never was.
    pub fn age(&self, at: u64) -> Option<u64> {
        (self.fetched_at != 0).then(|| at.saturating_sub(self.fetched_at))
    }

    /// Whether this account is held by a usage limit, and until when if the
    /// provider said. A recorded provider limit without a known exhausted
    /// window is blocked with an unknown end.
    pub fn limit_block(&self, at: u64) -> Option<Option<u64>> {
        let exhausted: Vec<&Window> = self
            .usage
            .windows
            .iter()
            .filter(|w| w.used_percent >= 100.0)
            .chain(self.unverified_limits.iter().map(UnverifiedLimit::window))
            .filter(|w| w.resets_at.is_none_or(|r| r > at))
            .collect();
        if exhausted.is_empty() && self.error.as_deref() != Some("provider-limit") {
            return None;
        }
        let known = exhausted.iter().all(|w| w.resets_at.is_some());
        Some(
            known
                .then(|| exhausted.iter().filter_map(|w| w.resets_at).max())
                .flatten(),
        )
    }

    pub fn reset_times(&self) -> impl Iterator<Item = u64> + '_ {
        self.usage
            .windows
            .iter()
            .chain(self.unverified_limits.iter().map(UnverifiedLimit::window))
            .filter_map(|window| window.resets_at)
    }

    // A successful poll can omit a window. Keep its hard limit separate from
    // the new measurements so neither a partial poll nor time passing clears it.
    pub(crate) fn preserve_limits(&mut self, old: &Self) {
        for (window, observed_at) in old
            .unverified_limits
            .iter()
            .map(|limit| (limit.window(), limit.observed_at(old.fetched_at)))
            .chain(
                old.usage
                    .windows
                    .iter()
                    .filter(|w| w.used_percent >= 100.0)
                    .map(|window| (window, old.fetched_at)),
            )
        {
            let current = |w: &&Window| w.resets_at.is_none_or(|r| r > self.fetched_at);
            let matching = |w: &Window| {
                w.name == window.name
                    && (window.name != "quota" || w.codex_slot == window.codex_slot)
            };
            let mut windows = self.usage.windows.iter().filter(current);
            let observed = self.fetched_at >= observed_at
                && if window.name == "quota" {
                    // A durationally anonymous Codex window can be verified in
                    // its own slot or by a fresh reading covering both slots.
                    windows
                        .clone()
                        .any(|w| window.codex_slot.is_some() && w.codex_slot == window.codex_slot)
                        || windows.nth(1).is_some()
                } else {
                    windows.any(matching)
                };
            if observed {
                continue;
            }
            let limit = UnverifiedLimit::Observed {
                window: window.clone(),
                observed_at,
            };
            match self
                .unverified_limits
                .iter_mut()
                .find(|retained| matching(retained.window()))
            {
                Some(retained) if retained.observed_at(self.fetched_at) < observed_at => {
                    *retained = limit;
                }
                Some(_) => (),
                None => self.unverified_limits.push(limit),
            }
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
                codex_slot: None,
            });
        }
    }
    if windows.is_empty() {
        return Err(Error("no supported Claude usage windows"));
    }
    Ok(Usage {
        windows,
        ..Default::default()
    })
}
pub fn parse_codex(v: &Value) -> Result<Usage> {
    let mut windows = Vec::new();
    // The standard codex bucket is comparable across profiles. Do not mix
    // unrelated model buckets into the account-selection metric.
    let bucket = v
        .pointer("/rateLimitsByLimitId/codex")
        .or_else(|| v.get("rateLimits"));
    if let Some(bucket) = bucket {
        for (slot, key) in [(0, "primary"), (1, "secondary")] {
            if let Some(w) = bucket.get(key).filter(|w| !w.is_null()) {
                let duration_minutes = w.get("windowDurationMins").and_then(Value::as_u64);
                windows.push(Window {
                    name: codex_window_name(duration_minutes),
                    used_percent: percent(w.get("usedPercent"))?,
                    resets_at: w.get("resetsAt").and_then(Value::as_u64),
                    duration_minutes,
                    codex_slot: Some(slot),
                });
            }
        }
    }
    let credits = bucket
        .and_then(|b| b.get("credits"))
        .filter(|c| c.is_object())
        .map(|c| Credits {
            // Only a bounded decimal balance is user-visible; never forward arbitrary RPC text.
            balance: c
                .get("balance")
                .and_then(Value::as_str)
                .filter(|s| {
                    !s.is_empty()
                        && s.len() <= 40
                        && s.bytes().all(|b| b.is_ascii_digit() || b == b'.')
                        && s.parse::<f64>().is_ok_and(|n| n.is_finite() && n >= 0.0)
                })
                .map(str::to_owned),
            unlimited: c.get("unlimited").and_then(Value::as_bool),
            automatic_reload: None,
        });
    let available_usage_resets = v
        .pointer("/rateLimitResetCredits/availableCount")
        .and_then(Value::as_u64);
    Ok(Usage {
        windows,
        credits,
        available_usage_resets,
        reset_credits: parse_reset_credits(v),
        reset_credits_listed: v
            .pointer("/rateLimitResetCredits/credits")
            .is_some_and(Value::is_array),
        ordinary_usage: OrdinaryUsage::from_read(v),
    })
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
    pub(crate) fn unavailable() -> Self {
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
    codex_request(
        profile,
        diagnostic,
        &[],
        "account/rateLimits/read",
        serde_json::json!({}),
    )
}
/// Shared bounded RPC transport. Responses and provider stderr never enter errors.
pub(crate) fn codex_request(
    profile: &Path,
    diagnostic: &std::cell::Cell<Failure>,
    overrides: &[String],
    method: &str,
    params: Value,
) -> Result<Value> {
    codex_request_within(
        profile,
        diagnostic,
        overrides,
        method,
        params,
        Duration::from_secs(10),
    )
}
/// `codex_request` with its own per-response deadline.
pub(crate) fn codex_request_within(
    profile: &Path,
    diagnostic: &std::cell::Cell<Failure>,
    overrides: &[String],
    method: &str,
    params: Value,
    timeout: Duration,
) -> Result<Value> {
    let mut command = Provider::Codex.command(profile);
    command.args(overrides);
    let mut child = Process(
        command
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
            let deadline = std::time::Instant::now() + timeout;
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
            serde_json::json!({"id":2,"method":method,"params":params}),
        )?;
        receive(2)
    })();
    drop(input);
    drop(child);
    let _ = reader.join();
    result
}
pub fn fetch(account: &Account, profile: &Path) -> std::result::Result<Usage, Failure> {
    account.provider.spec().usage(profile)
}
/// Quota through a usage-only Codex app-server, given access-only Codex auth.
pub(crate) fn fetch_codex(auth: Result<Value>) -> std::result::Result<Usage, Failure> {
    let diagnostic = std::cell::Cell::new(Failure::new("codex-rpc-unavailable"));
    let result = (|| -> Result<Value> {
        let temporary = tempfile::tempdir().safe("cannot prepare usage auth")?;
        store::json_write(&temporary.path().join(Provider::Codex.auth_file()), &auth?)?;
        codex_rpc_inner(temporary.path(), &diagnostic)
    })();
    result.and_then(|v| parse_codex(&v)).map_err(|e| {
        let mut failure = diagnostic.get();
        if e.0 == "cannot start Codex app-server" {
            failure.kind = "codex-rpc-start-failed";
        } else if e.0 == "app-server response timed out or disconnected" {
            failure.kind = "codex-rpc-timeout";
        }
        failure
    })
}
pub(crate) fn fetch_claude(profile: &Path) -> std::result::Result<Usage, Failure> {
    fetch_claude_with(profile, request_claude)
}
/// A temporary Codex home holding only the profile's access token: an
/// app-server started there cannot rotate auth behind a running CLI.
pub(crate) fn access_only_home(account: &Account, profile: &Path) -> Result<tempfile::TempDir> {
    let temporary = tempfile::tempdir().safe("cannot prepare usage auth")?;
    let auth: Value = store::json_read(&profile.join(account.provider.auth_file()))?;
    store::json_write(
        &temporary.path().join(account.provider.auth_file()),
        &crate::refresh::access_only(account.provider, &auth),
    )?;
    Ok(temporary)
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
        Ok(usage) => {
            let mut entry = Cached {
                usage,
                fetched_at: at,
                next_poll_at: at.saturating_add(TTL),
                last_attempt: at,
                ..Cached::default()
            };
            entry.preserve_limits(&old);
            entry
        }
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

fn read_cache(path: &Path, ignore_invalid_json: bool) -> Result<Cached> {
    if !path.exists() {
        return Ok(Cached::default());
    }
    if ignore_invalid_json {
        // A force poll can replace invalid quota data while retaining the
        // private-file checks and the atomic write after a fetch attempt.
        return match store::read_private(path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes).unwrap_or_default()),
            Err(Error(store::PRIVATE_FILE_TOO_LARGE)) => Ok(Cached::default()),
            Err(err) => Err(err),
        };
    }
    store::json_read(path)
}

/// How long an explicit (forced) read waits for another process's fetch of
/// the same account before returning the cache as it is.
const FORCED_WAIT: std::time::Duration = std::time::Duration::from_secs(20);

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
    // The decision and the cache writes happen under the store lock; the
    // fetch itself does not. Every supervisor takes the store lock on its
    // terminal thread, so a network call under it would stall typing in
    // every session. A usage lock per login keeps it one fetch at a time.
    let (old, path, profile) = match fresh_enough(store, account, at, force)? {
        Decision::Cached(cached) => return Ok(cached),
        Decision::Fetch { old, path, profile } => (old, path, profile),
    };
    let wait = if force {
        FORCED_WAIT
    } else {
        std::time::Duration::ZERO
    };
    let waiting = std::time::Instant::now();
    let Some(_fetching) = store.usage_lock(account, wait)? else {
        // Another process is fetching this login: use what it leaves.
        let _lock = store.lock()?;
        return read_cache(&path, force).or(Ok(old));
    };
    // Time moved on while waiting for another fetch.
    let at = at.saturating_add(waiting.elapsed().as_secs());
    // It may have just finished: decide again before fetching.
    let (snapshot, path) = match fresh_enough(store, account, at, force)? {
        Decision::Cached(cached) => return Ok(cached),
        Decision::Fetch { path, .. } => (read_cache(&path, force)?, path),
    };
    let observation = {
        let _lock = store.lock()?;
        let state = store.load()?;
        let floor = crate::identity::siblings(store, account, &state.accounts)
            .iter()
            .filter_map(|other| {
                let path = store
                    .profile(other.provider, &other.label)
                    .ok()?
                    .join("usage-cache.json");
                read_cache(&path, false).ok()
            })
            .chain(read_cache(&path, false).ok())
            .map(|cache| cache.observation)
            .max()
            .unwrap_or(0);
        next_observation(store, floor)?
    };
    let result = fetcher(&profile);
    let _lock = store.lock()?;
    // The account may have been removed or renamed during the fetch; writing
    // would recreate its old profile directory and block that label.
    let state = store.load()?;
    if store
        .account(&state, account.provider, Some(&account.label))
        .is_err()
    {
        return Ok(updated(snapshot, result, at));
    }
    let latest = read_cache(&path, force)?;
    // Compare-and-set: an observation started after this fetch landed here
    // during it (another program's, or an older aswap's, which has no
    // number and is ordered by time). This one is older: it loses.
    let started = Cached {
        fetched_at: at,
        observation,
        ..Cached::default()
    };
    if latest.newer_than(&snapshot) && latest.newer_than(&started) {
        return Ok(latest);
    }
    let mut entry = merged(&snapshot, latest, result, at);
    if entry.fetched_at == at && entry.error.is_none() {
        entry.observation = observation;
    }
    store::json_write(&path, &entry)?;
    share_reading(store, account, &state.accounts, &entry, at)?;
    Ok(entry)
}
/// A good reading is the real account's: hand it to the same account in
/// other programs whose reading is older, so a waiter there sees a reset
/// (or a limit) now rather than at its own next poll, and no program polls
/// for what another just read. Caller holds the store lock.
fn share_reading(
    store: &Store,
    account: &Account,
    accounts: &[Account],
    entry: &Cached,
    at: u64,
) -> Result<()> {
    if entry.error.is_some() || entry.used(at).is_none() {
        return Ok(());
    }
    for other in crate::identity::bindings(store, account, accounts) {
        let path = store
            .profile(other.provider, &other.label)?
            .join("usage-cache.json");
        let old = read_cache(&path, false).unwrap_or_default();
        if !entry.newer_than(&old) {
            continue;
        }
        store::json_write(&path, &old.adopt(entry))?;
    }
    Ok(())
}
/// The cache after a fetch that started from `snapshot`, when `latest` is
/// what is stored now. Writes made during the fetch win where they carry
/// news the fetch can't have seen: a limit the provider reported
/// (mark_limited), and a rotated login that clears an auth backoff
/// (credentials_changed).
pub(crate) fn merged(
    snapshot: &Cached,
    latest: Cached,
    result: std::result::Result<Usage, Failure>,
    at: u64,
) -> Cached {
    let changed = serde_json::to_value(snapshot).ok() != serde_json::to_value(&latest).ok();
    let marked = (changed && latest.error.as_deref() == Some("provider-limit")).then(|| {
        (
            latest.error.clone(),
            latest.http_status,
            latest.retry_after,
            latest.next_poll_at,
        )
    });
    let retry_now = changed && latest.next_poll_at < snapshot.next_poll_at;
    let failed = result.is_err();
    let mut entry = updated(latest, result, at);
    if let Some((error, http_status, retry_after, next_poll_at)) = marked {
        entry.error = error;
        entry.http_status = http_status;
        entry.retry_after = retry_after;
        entry.next_poll_at = entry.next_poll_at.max(next_poll_at);
    } else if failed && retry_now {
        // The failure may have used the login that was just replaced.
        entry.next_poll_at = at;
    }
    entry
}
enum Decision {
    Cached(Cached),
    Fetch {
        old: Cached,
        path: PathBuf,
        profile: PathBuf,
    },
}
/// Under the store lock: the cache if it is still good (or a fresh alias's
/// reading, which it adopts), else what a fetch needs.
fn fresh_enough(store: &Store, account: &Account, at: u64, force: bool) -> Result<Decision> {
    let _lock = store.lock()?;
    let state = store.load()?;
    store.account(&state, account.provider, Some(&account.label))?;
    let profile = store.profile(account.provider, &account.label)?;
    let path = profile.join("usage-cache.json");
    let mut old = read_cache(&path, force)?;
    if old.error.is_some() && old.retry_after.is_none() {
        old.next_poll_at = old
            .next_poll_at
            .min(old.last_attempt.max(old.fetched_at).saturating_add(900));
    }
    let reset_due = old
        .reset_times()
        .any(|r| r <= at && old.last_attempt.max(old.fetched_at) < r);
    if !force && !reset_due && at < old.next_poll_at {
        return Ok(Decision::Cached(old));
    }
    // One real account has one quota, whichever program or label reads it:
    // reuse a fresh sibling observation (an alias, or the same account in
    // another program) instead of fetching again, which would double the
    // provider's rate limit.
    if !force {
        for sibling in crate::identity::siblings(store, account, &state.accounts) {
            let sibling = store
                .profile(sibling.provider, &sibling.label)?
                .join("usage-cache.json");
            if !sibling.exists() {
                continue;
            }
            let other: Cached = store::json_read(&sibling)?;
            if other.error.is_none()
                && other.newer_than(&old)
                && at < other.fetched_at.saturating_add(TTL)
                && other.used(at).is_some()
                && !old.own_failure()
            {
                let adopted = old.adopt(&other);
                if adopted.used(at).is_none() {
                    continue;
                }
                store::json_write(&path, &adopted)?;
                return Ok(Decision::Cached(adopted));
            }
        }
    }
    Ok(Decision::Fetch { old, path, profile })
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
        let mut cached = read_cache(&path, force)?;
        if result.is_err() {
            cached.error = Some("sync-unavailable".into());
            cached.http_status = None;
        }
        return Ok(cached);
    }

    // A refresh rewrites shared state (views, aliases), so it runs under
    // the store lock; it is due rarely, unlike the fetch around it.
    let refresh = |force: bool| {
        let _lock = store
            .lock()
            .map_err(|_| Failure::new("usage-unavailable"))?;
        crate::refresh::refresh(store, account, now(), force)
    };
    cached_mode_with(store, account, now(), force, |p| {
        fetch_refreshing(account, p, refresh, |p| fetch(account, p))
    })
}
/// One usage fetch, refreshing the login first when it is due and forcing
/// one retry after a refresh only when `refresh_warranted` says the failure
/// was about auth.
fn fetch_refreshing(
    account: &Account,
    profile: &Path,
    refresh: impl Fn(bool) -> std::result::Result<bool, Failure>,
    fetch: impl Fn(&Path) -> std::result::Result<Usage, Failure>,
) -> std::result::Result<Usage, Failure> {
    refresh(false)?;
    let first = fetch(profile);
    if first
        .as_ref()
        .err()
        .is_some_and(|e| refresh_warranted(account.provider, profile, e, now()))
        && refresh(true)?
    {
        fetch(profile)
    } else {
        first
    }
}
/// Whether a failed fetch justifies forcing a token refresh. A forced
/// refresh skips the backoff gate and rotates the login, which revokes the
/// access token of every running view of it, so only an auth failure counts:
/// an explicit 401, or a Codex app-server that failed while the stored access
/// token had expired. A changed response shape, an app-server that exits
/// early, or any other transport failure never rotates.
pub(crate) fn refresh_warranted(
    provider: Provider,
    profile: &Path,
    failure: &Failure,
    at: u64,
) -> bool {
    if failure.status == Some(401) {
        return true;
    }
    provider == Provider::Codex
        && failure.kind == "codex-rpc-unavailable"
        && store::json_read::<Value>(&profile.join(provider.auth_file()))
            .ok()
            .and_then(|auth| crate::refresh::expires_at(provider, &auth))
            .is_some_and(|expiry| expiry <= at)
}
/// Ranks two eligible accounts: soonest weekly reset first under
/// `WeeklyReset` (unknown resets last), then most headroom.
fn rank(prefer: Prefer, a: &Cached, b: &Cached, at: u64) -> std::cmp::Ordering {
    let by_reset = match prefer {
        Prefer::Headroom => std::cmp::Ordering::Equal,
        Prefer::WeeklyReset => match (a.usage.weekly_reset(at), b.usage.weekly_reset(at)) {
            (Some(x), Some(y)) => x.cmp(&y),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        },
    };
    by_reset.then_with(|| a.used(at).unwrap().total_cmp(&b.used(at).unwrap()))
}
/// Prefer fresh quotas below the threshold with hysteresis and cooldown.
/// An exhausted account may use remaining quota above the soft threshold.
pub fn choose<'a>(
    current: &str,
    entries: &'a [(Account, Cached)],
    settings: &store::Auto,
    at: u64,
    limited: bool,
    pinned: Option<&str>,
) -> Option<&'a Account> {
    let threshold = settings.threshold;
    let (_, current_cache) = entries.iter().find(|(a, _)| a.label == current)?;
    let used = current_cache.used(at);
    let exhausted = limited
        || !settings.allows(current, pinned)
        || !current_cache.unverified_limits.is_empty()
        || used.is_some_and(|v| v >= 100.0);
    if !exhausted
        && (pinned == Some(current)
            || used.is_none_or(|v| v < threshold || at.saturating_sub(settings.last_switch) < 300))
    {
        return None;
    }
    let eligible = |ceiling: f64, forced: bool, preference: Prefer| {
        entries
            .iter()
            .filter(|(a, c)| {
                a.label != current
                    && settings.allows(&a.label, pinned)
                    && c.used(at).is_some_and(|v| {
                        v < ceiling && (forced || used.is_some_and(|u| u - v >= 10.0))
                    })
            })
            .min_by(|(_, a), (_, b)| rank(preference, a, b, at))
            .map(|(a, _)| a)
    };
    eligible(threshold, exhausted, settings.prefer).or_else(|| {
        exhausted
            .then(|| eligible(100.0, true, Prefer::Headroom))
            .flatten()
    })
}
/// A switch that is not forced by quota: with `WeeklyReset`, a new launch (or
/// the selection monitor) moves to an eligible account whose weekly window
/// resets strictly sooner than the current one's. Running sessions are never
/// interrupted for this; only `choose` stops a child.
pub fn prefer_at_launch<'a>(
    current: &str,
    entries: &'a [(Account, Cached)],
    settings: &store::Auto,
    at: u64,
) -> Option<&'a Account> {
    let prefer = settings.prefer;
    if prefer != Prefer::WeeklyReset {
        return None;
    }
    let (_, mine) = entries.iter().find(|(a, _)| a.label == current)?;
    let best = entries
        .iter()
        .filter(|(a, c)| {
            settings.allows(&a.label, None) && c.used(at).is_some_and(|v| v < settings.threshold)
        })
        .min_by(|(_, a), (_, b)| rank(prefer, a, b, at))?;
    let sooner = match (best.1.usage.weekly_reset(at), mine.usage.weekly_reset(at)) {
        (Some(x), Some(y)) => x < y,
        (Some(_), None) => true,
        _ => false,
    };
    (best.0.label != current && sooner).then_some(&best.0)
}

/// For a one-call `run --pick`: `choose` keeps a current account whose quota
/// is unknown, as a supervised launch should. A single call is better off
/// elsewhere when the current account's reading is an error (an expired
/// login) or its last reading is over the threshold: it takes the best
/// account whose last error-free reading is below the threshold. Readings
/// may be stale here (a follower's come from the owner, often older than the
/// freshness limit); a stale reading still tells a live account from a dead
/// one. Reserved accounts are skipped unless `pinned`.
pub fn best_known<'a>(
    current: &str,
    entries: &'a [(Account, Cached)],
    settings: &store::Auto,
    at: u64,
    pinned: Option<&str>,
) -> Option<&'a Account> {
    let (threshold, prefer) = (settings.threshold, settings.prefer);
    // An exhausted window missing from the last poll may still be exhausted.
    let known = |c: &Cached| {
        (c.error.is_none() && c.unverified_limits.is_empty())
            .then(|| c.usage.used(at))
            .flatten()
    };
    let (_, mine) = entries.iter().find(|(a, _)| a.label == current)?;
    if settings.allows(current, pinned)
        && (mine.used(at).is_some()
            || (mine.error.is_none() && known(mine).is_some_and(|v| v < threshold)))
    {
        return None;
    }
    entries
        .iter()
        .filter_map(|(a, c)| Some((a, c, known(c).filter(|v| *v < threshold)?)))
        .filter(|(a, _, _)| a.label != current && settings.allows(&a.label, pinned))
        .min_by(|(_, a, x), (_, b, y)| {
            let by_reset = match prefer {
                Prefer::Headroom => std::cmp::Ordering::Equal,
                Prefer::WeeklyReset => match (a.usage.weekly_reset(at), b.usage.weekly_reset(at)) {
                    (Some(p), Some(q)) => p.cmp(&q),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => std::cmp::Ordering::Equal,
                },
            };
            by_reset.then_with(|| x.total_cmp(y))
        })
        .map(|(a, _, _)| a)
}
/// A provider rejected this session's credentials; preserve quota as last-known.
pub fn mark_auth_expired(store: &Store, account: &Account) -> Result<()> {
    let _lock = store.lock()?;
    let path = store
        .profile(account.provider, &account.label)?
        .join("usage-cache.json");
    let mut entry: Cached = if path.exists() {
        store::json_read(&path)?
    } else {
        Cached::default()
    };
    entry.error = Some("http-401".into());
    entry.http_status = Some(401);
    entry.last_attempt = now();
    entry.next_poll_at = now().saturating_add(TTL);
    store::json_write(&path, &entry)
}

pub fn mark_limited(store: &Store, account: &Account) -> Result<()> {
    let _lock = store.lock()?;
    mark_limited_unlocked(store, account)?;
    // One real account, one quota: the same account in another program hit
    // the same limit (its own aliases are covered by its own run).
    let state = store.load()?;
    for other in crate::identity::bindings(store, account, &state.accounts) {
        mark_limited_unlocked(store, &other)?;
    }
    Ok(())
}
fn mark_limited_unlocked(store: &Store, account: &Account) -> Result<()> {
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
    // Invalid quota data has no backoff to clear and must not prevent token
    // refresh ahead of the poll that repairs the cache.
    let mut cache = read_cache(&path, true)?;
    if cache.http_status == Some(401)
        || cache
            .error
            .as_deref()
            .is_some_and(|s| matches!(s, "http-401" | "refresh-invalid-grant" | "refresh-http-401"))
    {
        cache.next_poll_at = now();
        store::json_write(&path, &cache)?;
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
    #[test]
    fn a_single_call_leaves_an_account_of_unknown_quota_for_a_known_one() {
        let at = 10_000;
        let headroom = || store::Auto {
            threshold: 90.0,
            prefer: Prefer::Headroom,
            ..Default::default()
        };
        let entry = |label: &str, used: Option<f64>| {
            let mut cache = Cached {
                fetched_at: at,
                ..Default::default()
            };
            match used {
                Some(used) => cache.usage.windows.push(Window {
                    name: "five_hour".into(),
                    used_percent: used,
                    resets_at: None,
                    duration_minutes: Some(300),
                    codex_slot: None,
                }),
                None => cache.error = Some("http-401".into()),
            }
            (
                Account {
                    provider: Provider::Claude,
                    label: label.into(),
                },
                cache,
            )
        };
        let entries = vec![
            entry("expired", None),
            entry("full", Some(95.0)),
            entry("busy", Some(40.0)),
            entry("free", Some(10.0)),
        ];
        let pick = |current| {
            best_known(current, &entries, &headroom(), at, None).map(|a| a.label.as_str())
        };
        assert_eq!(pick("expired"), Some("free"));
        // A known current account is left to `choose`.
        assert_eq!(pick("busy"), None);
        assert_eq!(pick("full"), None);
        // Stale but error-free readings still count; a stale current account
        // below the threshold stays.
        let mut stale = entries.clone();
        for (_, cache) in &mut stale {
            cache.fetched_at = at - 3600;
        }
        let pick_stale =
            |current| best_known(current, &stale, &headroom(), at, None).map(|a| a.label.as_str());
        assert_eq!(pick_stale("expired"), Some("free"));
        assert_eq!(pick_stale("full"), Some("free"));
        assert_eq!(pick_stale("busy"), None);
        // Nothing known below the threshold: stay.
        let only = vec![entry("expired", None), entry("full", Some(95.0))];
        assert!(best_known("expired", &only, &headroom(), at, None).is_none());
        // A weekly limit the last poll left out keeps an account unknown.
        let (_, mut exhausted) = entry("free", Some(10.0));
        exhausted.fetched_at = at - 60;
        exhausted.usage.windows[0] = Window {
            name: "seven_day".into(),
            used_percent: 100.0,
            resets_at: None,
            duration_minutes: Some(10080),
            codex_slot: None,
        };
        let partial = entry("free", Some(10.0)).1.usage;
        let mut unverified = entries.clone();
        unverified[3].1 = updated(exhausted, Ok(partial), at);
        assert!(!unverified[3].1.unverified_limits.is_empty());
        assert_eq!(
            best_known("expired", &unverified, &headroom(), at, None).map(|a| a.label.as_str()),
            Some("busy")
        );
        // A reserved account is never picked, unless it was asked for.
        let mut reserved = headroom();
        reserved.reserved = vec!["free".into()];
        assert_eq!(
            best_known("expired", &entries, &reserved, at, None).map(|a| a.label.as_str()),
            Some("busy")
        );
        assert_eq!(
            best_known("expired", &entries, &reserved, at, Some("free")).map(|a| a.label.as_str()),
            Some("free")
        );
    }
}

#[cfg(test)]
mod reason_tests {
    use super::*;

    const AT: u64 = 100_000;

    fn window(used: f64, reset: Option<u64>) -> Window {
        Window {
            name: "weekly".into(),
            used_percent: used,
            resets_at: reset,
            duration_minutes: Some(10080),
            codex_slot: None,
        }
    }
    fn reading(fetched_at: u64, windows: Vec<Window>) -> Cached {
        Cached {
            usage: Usage {
                windows,
                ..Default::default()
            },
            fetched_at,
            ..Default::default()
        }
    }

    #[test]
    fn every_reason_and_only_fresh_is_usable() {
        let fresh = reading(AT - 10, vec![window(40.0, Some(AT + 60))]);
        let mut error = fresh.clone();
        error.error = Some("http-401".into());
        let never = Cached::default();
        let stale = reading(AT - STALE_AFTER - 1, vec![window(40.0, None)]);
        let reset = reading(AT - 10, vec![window(100.0, Some(AT))]);
        let empty = reading(AT - 10, Vec::new());
        let old = reading(AT - 60, vec![window(100.0, Some(AT + 600))]);
        let unverified = updated(old, Ok(Usage::default()), AT - 10);
        assert!(!unverified.unverified_limits.is_empty());
        let cases = [
            (fresh, Reason::Fresh, Some(10)),
            (error, Reason::Error, Some(10)),
            (never, Reason::NeverFetched, None),
            (stale, Reason::Stale, Some(STALE_AFTER + 1)),
            (reset, Reason::ResetPassed, Some(10)),
            (empty, Reason::NoWindows, Some(10)),
            (unverified, Reason::UnverifiedLimit, Some(10)),
        ];
        for (cache, reason, age) in cases {
            assert_eq!(cache.reason(AT), reason);
            assert_eq!(cache.age(AT), age, "{reason:?}");
            // The reason agrees with what selection uses.
            assert_eq!(
                cache.used(AT).is_some(),
                reason == Reason::Fresh,
                "{reason:?}"
            );
        }
    }

    #[test]
    fn reasons_serialize_as_the_documented_strings() {
        let names: Vec<_> = [
            Reason::Fresh,
            Reason::NeverFetched,
            Reason::Error,
            Reason::UnverifiedLimit,
            Reason::Stale,
            Reason::ResetPassed,
            Reason::NoWindows,
        ]
        .iter()
        .map(|r| serde_json::to_value(r).unwrap())
        .collect();
        assert_eq!(
            names,
            [
                "fresh",
                "never-fetched",
                "error",
                "unverified-limit",
                "stale",
                "reset-passed",
                "no-windows"
            ]
        );
    }
}
#[cfg(test)]
mod merge_tests {
    use super::*;

    fn reading(used: f64) -> Usage {
        Usage {
            windows: vec![Window {
                name: "five_hour".into(),
                used_percent: used,
                resets_at: None,
                duration_minutes: Some(300),
                codex_slot: None,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn writes_made_during_a_fetch_are_kept() {
        let at = 10_000;
        let snapshot = Cached {
            next_poll_at: at,
            ..Cached::default()
        };
        // Nothing changed meanwhile: the fetch's result as is.
        let plain = merged(&snapshot, snapshot.clone(), Ok(reading(5.0)), at);
        assert_eq!(plain.used(at), Some(5.0));
        assert_eq!(plain.next_poll_at, at + TTL);
        // The provider reported a limit meanwhile (mark_limited): kept over
        // a reading that predates it.
        let mut limited = snapshot.clone();
        limited.error = Some("provider-limit".into());
        limited.next_poll_at = at + 2 * TTL;
        let kept = merged(&snapshot, limited, Ok(reading(5.0)), at);
        assert_eq!(kept.error.as_deref(), Some("provider-limit"));
        assert_eq!(kept.next_poll_at, at + 2 * TTL);
        // The login rotated meanwhile (credentials_changed lowered the next
        // poll): a failure, possibly with the old token, retries at once.
        let mut stale = snapshot.clone();
        stale.error = Some("http-401".into());
        stale.next_poll_at = at + 900;
        let mut rotated = stale.clone();
        rotated.next_poll_at = at;
        let retried = merged(&stale, rotated, Err(Failure::http(401, 0)), at);
        assert_eq!(retried.next_poll_at, at);
        // Without that, a failure backs off as usual.
        let backed = merged(&stale, stale.clone(), Err(Failure::http(401, 0)), at);
        assert!(backed.next_poll_at > at);
    }
}
#[cfg(test)]
mod rotation_tests {
    use super::*;
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};

    /// A profile whose Codex access token (a synthetic JWT) expires at `exp`.
    fn codex_profile(exp: u64) -> tempfile::TempDir {
        let t = tempfile::tempdir().unwrap();
        let payload = URL_SAFE_NO_PAD.encode(format!("{{\"exp\":{exp}}}"));
        store::json_write(
            &t.path().join("auth.json"),
            &serde_json::json!({"tokens":{"access_token":format!("e30.{payload}.sig"),"refresh_token":"synthetic-r"}}),
        )
        .unwrap();
        t
    }

    #[test]
    fn only_auth_failures_force_a_rotation() {
        let at = 10_000;
        let live = codex_profile(at + 3600);
        let expired = codex_profile(at - 1);
        let unavailable = Failure::new("codex-rpc-unavailable");
        let warranted = |p: &tempfile::TempDir, f: &Failure| {
            refresh_warranted(Provider::Codex, p.path(), f, at)
        };
        // A new response shape, an early exit or "too many notifications"
        // all surface as codex-rpc-unavailable: no rotation while the
        // token is still valid.
        assert!(!warranted(&live, &unavailable));
        assert!(warranted(&expired, &unavailable));
        assert!(warranted(&live, &Failure::http(401, 0)));
        for kind in [
            "codex-rpc-timeout",
            "codex-rpc-start-failed",
            "usage-unavailable",
        ] {
            assert!(!warranted(&expired, &Failure::new(kind)), "{kind}");
        }
        assert!(!warranted(&live, &Failure::http(429, 60)));
        assert!(!warranted(&live, &Failure::http(500, 0)));
        // No readable login: nothing to judge by, so no rotation.
        let empty = tempfile::tempdir().unwrap();
        assert!(!warranted(&empty, &unavailable));
        // Claude: only an explicit 401.
        assert!(refresh_warranted(
            Provider::Claude,
            empty.path(),
            &Failure::http(401, 0),
            at
        ));
        assert!(!refresh_warranted(
            Provider::Claude,
            empty.path(),
            &unavailable,
            at
        ));
    }

    #[test]
    fn a_fetch_forces_a_refresh_only_when_warranted() {
        let codex = Account {
            provider: Provider::Codex,
            label: "synthetic".into(),
        };
        // (stored token expiry, the fetch's failure) -> forced refreshes.
        let forced = |exp: u64, failure: Failure| {
            let profile = codex_profile(exp);
            let forced = std::cell::Cell::new(0);
            let _ = fetch_refreshing(
                &codex,
                profile.path(),
                |force| {
                    forced.set(forced.get() + u32::from(force));
                    Ok(force)
                },
                |_| Err(failure),
            );
            forced.get()
        };
        let live = now() + 3600;
        assert_eq!(forced(live, Failure::new("codex-rpc-unavailable")), 0);
        assert_eq!(forced(live, Failure::new("codex-rpc-timeout")), 0);
        assert_eq!(forced(now() - 1, Failure::new("codex-rpc-unavailable")), 1);
        assert_eq!(forced(live, Failure::http(401, 0)), 1);
    }
}
#[cfg(test)]
mod read_contract_tests {
    use super::*;

    fn read(extra: serde_json::Value) -> Usage {
        let mut v = serde_json::json!({"rateLimits":{"primary":{"usedPercent":10,"windowDurationMins":300,"resetsAt":2000000000}}});
        v.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        parse_codex(&v).unwrap()
    }

    #[test]
    fn ordinary_usage_permission_is_kept_as_reported() {
        assert_eq!(
            read(serde_json::json!({})).ordinary_usage,
            OrdinaryUsage::NotReported
        );
        let null = read(serde_json::json!({"ordinaryUsageAllowed": null}));
        assert_eq!(null.ordinary_usage, OrdinaryUsage::Unknown);
        assert!(!null.ordinary_usage.permits_recovery());
        let denied = read(serde_json::json!({"ordinaryUsageAllowed": false}));
        assert_eq!(denied.ordinary_usage, OrdinaryUsage::Denied);
        assert!(!denied.ordinary_usage.permits_recovery());
        let allowed = read(serde_json::json!({"ordinaryUsageAllowed": true}));
        assert!(allowed.ordinary_usage.permits_recovery());
        assert!(OrdinaryUsage::NotReported.permits_recovery());
    }

    #[test]
    fn credit_details_null_and_empty_stay_distinct() {
        let counted =
            read(serde_json::json!({"rateLimitResetCredits":{"availableCount":2,"credits":null}}));
        assert_eq!(counted.available_usage_resets, Some(2));
        assert!(!counted.reset_credits_listed);
        let empty =
            read(serde_json::json!({"rateLimitResetCredits":{"availableCount":0,"credits":[]}}));
        assert!(empty.reset_credits_listed && empty.reset_credits.is_empty());
        let none = read(serde_json::json!({}));
        assert_eq!(none.available_usage_resets, None);
        assert!(!none.reset_credits_listed);
    }
}
#[cfg(test)]
mod permission_selection_tests {
    use super::*;

    #[test]
    fn a_denied_or_unknown_permission_is_never_chosen() {
        let at = 1_800_000_000;
        let reading = |used: f64, ordinary: OrdinaryUsage| Cached {
            usage: Usage {
                windows: vec![Window {
                    name: "weekly".into(),
                    used_percent: used,
                    resets_at: Some(at + 86_400),
                    duration_minutes: Some(10080),
                    codex_slot: Some(1),
                }],
                ordinary_usage: ordinary,
                ..Default::default()
            },
            fetched_at: at,
            ..Default::default()
        };
        let account = |label: &str| Account {
            provider: Provider::Codex,
            label: label.into(),
        };
        let settings = store::Auto::default();
        for (ordinary, chosen) in [
            (OrdinaryUsage::Allowed, true),
            (OrdinaryUsage::NotReported, true),
            (OrdinaryUsage::Denied, false),
            (OrdinaryUsage::Unknown, false),
        ] {
            let entries = vec![
                (
                    account("current"),
                    reading(100.0, OrdinaryUsage::NotReported),
                ),
                (account("other"), reading(5.0, ordinary)),
            ];
            let pick = choose("current", &entries, &settings, at, true, None);
            assert_eq!(pick.is_some(), chosen, "{ordinary:?}");
        }
    }
}
