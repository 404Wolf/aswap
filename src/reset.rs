//! Opt-in redemption of Codex usage-limit reset credits.
//!
//! A Codex account can hold earned, expiring "usage limit reset" credits (in
//! the Codex TUI: `/usage`, then "Redeem reset"). Redeeming one resets that
//! account's eligible windows: the 5-hour and the weekly window together.
//! The credits are a limited allowance, so aswap spends one only when every
//! Codex account is out of weekly usage, and then on the account whose weekly
//! window resets farthest away, the one that would otherwise stay blocked
//! longest.
//!
//! [`decide`] is the pure rule. [`step`] runs one decision against a [`Port`]
//! (usage reads, the redeem call and persistence), which tests replace with a
//! fake. [`tick`] wires the port to the store and the Codex app-server; it
//! calls the same `account/rateLimitResetCredit/consume` request the Codex
//! TUI makes.
use crate::{
    Error, Result, Safe,
    provider::Provider,
    store::{self, Account, Auto, AutoReset, Store},
    usage::{self, Cached, ResetCredit},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{path::PathBuf, time::Duration};

/// No new decision for this long after a redemption that may have spent a credit.
pub const COOLDOWN: u64 = 3600;
/// Redeem attempts for one decision before giving up until that weekly window passes.
pub const MAX_ATTEMPTS: u32 = 3;
/// First retry delay after a failed attempt; doubles per attempt.
pub const RETRY_BASE: u64 = 900;
/// Records kept in the reset log.
pub const HISTORY: usize = 50;
/// The app-server bounds its backend call at 10 s; wait a little longer.
const RPC_TIMEOUT: Duration = Duration::from_secs(20);
const METHOD: &str = "account/rateLimitResetCredit/consume";

/// What aswap would redeem, and why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ResetDecision {
    pub label: String,
    #[serde(skip)]
    pub credit_id: String,
    pub weekly_resets_at: u64,
    pub credit_expires_at: Option<u64>,
    pub reason: String,
}

/// Why no reset is made.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "reason", rename_all = "kebab-case")]
pub enum Hold {
    /// `auto_reset` is off.
    Off,
    /// Only Codex has usage-limit resets.
    Unsupported,
    /// A sync follower never redeems; the refresh owner decides.
    Follower,
    /// A reset offer or response mentioned money; cleared by `reset off`.
    Halted,
    NoAccounts,
    Cooldown {
        until: u64,
    },
    /// This account's usage, or its weekly window, is unknown or stale.
    UsageUnknown {
        label: String,
    },
    /// This account still has weekly usage left.
    Headroom {
        label: String,
    },
    /// No eligible account holds an available reset credit.
    NoCredit,
    /// The fresh pre-redeem read no longer selects the same account.
    Changed {
        label: String,
    },
}

/// One reset-log entry. Idempotency keys and credit ids are never recorded here.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub at: u64,
    pub label: String,
    /// `dry-run`, `reset`, `already-redeemed`, `nothing-to-reset`, `no-credit`,
    /// `failed`, `gave-up`, `unconfirmed`, `refused` or `refused-response`.
    pub outcome: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weekly_resets_at: Option<u64>,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre: Option<Snapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post: Option<Snapshot>,
}
impl Record {
    /// Whether this attempt may have spent a credit, which starts the cooldown.
    fn may_have_spent(&self) -> bool {
        matches!(
            self.outcome.as_str(),
            "reset" | "already-redeemed" | "gave-up" | "unconfirmed" | "refused-response"
        )
    }
}

/// The quota around a redemption, without identity or credit ids.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub windows: Vec<SnapshotWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available_resets: Option<u64>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SnapshotWindow {
    pub name: String,
    pub used_percent: f64,
    pub resets_at: Option<u64>,
}
impl Snapshot {
    pub fn of(cache: &Cached) -> Self {
        Self {
            windows: cache
                .usage
                .windows
                .iter()
                .map(|w| SnapshotWindow {
                    name: w.name.clone(),
                    used_percent: w.used_percent,
                    resets_at: w.resets_at,
                })
                .collect(),
            available_resets: cache.usage.available_usage_resets,
        }
    }
}

/// A redemption persisted before its request, so a retry after a crash or
/// timeout reuses the idempotency key and cannot spend a second credit.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Pending {
    pub label: String,
    pub credit_id: String,
    pub idempotency_key: String,
    pub weekly_resets_at: u64,
    pub reason: String,
    pub started_at: u64,
    pub attempts: u32,
    pub retry_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre: Option<Snapshot>,
}

/// An account not to redeem for again before its weekly window resets.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skip {
    pub label: String,
    pub until: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DryRunMark {
    pub label: String,
    pub weekly_resets_at: u64,
}

/// Persistent reset state for Codex (`codex-resets.json` in the store).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ResetLog {
    pub pending: Option<Pending>,
    pub history: Vec<Record>,
    pub skipped: Vec<Skip>,
    /// Why resets stopped; set when an offer or response mentions money.
    pub halted: Option<String>,
    pub last_dry_run: Option<DryRunMark>,
}
impl ResetLog {
    fn push(&mut self, record: Record) {
        self.history.push(record);
        let excess = self.history.len().saturating_sub(HISTORY);
        self.history.drain(..excess);
    }
    fn cooldown_until(&self) -> Option<u64> {
        self.history
            .iter()
            .rev()
            .find(|r| r.may_have_spent())
            .map(|r| r.at.saturating_add(COOLDOWN))
    }
    fn skipped(&self, label: &str, at: u64) -> bool {
        self.skipped
            .iter()
            .any(|s| s.label == label && s.until > at)
    }
    fn skip(&mut self, label: &str, until: u64) {
        self.skipped.retain(|s| s.label != label);
        self.skipped.push(Skip {
            label: label.to_owned(),
            until,
        });
    }
    fn prune(&mut self, at: u64) {
        self.skipped.retain(|s| s.until > at);
    }
    /// Records a dropped pending redemption; see `drop_pending`.
    fn abandon(&mut self, pending: Pending, why: &str, at: u64) {
        self.pending = None;
        let sent = pending.attempts > 0;
        if sent {
            self.skip(&pending.label, pending.weekly_resets_at);
        }
        self.push(Record {
            at,
            label: pending.label,
            outcome: if sent { "unconfirmed" } else { "abandoned" }.into(),
            weekly_resets_at: Some(pending.weekly_resets_at),
            reason: if sent {
                format!("dropped a redeem request that got no answer: {why}")
            } else {
                format!("dropped a redeem request before sending it: {why}")
            },
            pre: pending.pre,
            post: None,
        });
    }
    /// `reset off`: clears a halt and drops any pending redemption, recording
    /// it. Returns whether anything changed.
    pub fn turn_off(&mut self, at: u64) -> bool {
        let halted = self.halted.take().is_some();
        let pending = self.pending.take();
        let changed = halted || pending.is_some();
        if let Some(pending) = pending {
            self.abandon(pending, "usage resets were turned off", at);
        }
        changed
    }
    /// The log without idempotency keys or credit ids, for status output.
    pub fn summary(&self) -> Value {
        serde_json::json!({
            "pending": self.pending.as_ref().map(|p| serde_json::json!({
                "label": p.label,
                "attempts": p.attempts,
                "retry_at": p.retry_at,
                "started_at": p.started_at,
            })),
            "halted": self.halted,
            "skipped": self.skipped,
            "history": self.history.iter().rev().take(10).collect::<Vec<_>>(),
        })
    }
}

const MONEY_PREFIXES: &[&str] = &[
    "purchas", "pay", "paid", "bill", "invoic", "upgrad", "buy", "bought", "charg", "pric", "cost",
    "subscri", "credit", "checkout", "money", "refund", "topup", "spend", "overage", "reserve",
];
const MONEY_WORDS: &[&str] = &["fee", "fees", "usd", "eur", "gbp", "card"];
const MONEY_PHRASES: &[&str] = &["extra usage", "add credit", "top up"];

/// Whether provider text mentions credits, purchases, payment, billing,
/// upgrades or extra usage. Errs on the side of refusing.
pub fn mentions_money(text: &str) -> bool {
    if text.chars().any(|c| matches!(c, '$' | '€' | '£' | '¥')) {
        return true;
    }
    let lower = text.to_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    let joined = words.join(" ");
    words.iter().any(|w| {
        MONEY_WORDS.contains(w) || MONEY_PREFIXES.iter().any(|prefix| w.starts_with(prefix))
    }) || MONEY_PHRASES.iter().any(|phrase| joined.contains(phrase))
}
fn credit_mentions_money(credit: &ResetCredit) -> bool {
    [&credit.title, &credit.description]
        .into_iter()
        .flatten()
        .any(|text| mentions_money(text))
}
/// Any string in a response, outside its `outcome` code, that mentions money.
fn response_mentions_money(v: &Value) -> bool {
    match v {
        Value::String(s) => mentions_money(s),
        Value::Array(items) => items.iter().any(response_mentions_money),
        Value::Object(map) => map
            .iter()
            .any(|(key, value)| key != "outcome" && response_mentions_money(value)),
        _ => false,
    }
}

/// Every Codex account must be known to be out of weekly usage.
fn guard(entries: &[(Account, Cached)], at: u64) -> std::result::Result<usize, Hold> {
    let mut count = 0;
    for (account, cache) in entries
        .iter()
        .filter(|(a, _)| a.provider == Provider::Codex)
    {
        count += 1;
        let weekly = cache
            .used(at)
            .and_then(|_| cache.usage.weekly(at))
            .ok_or_else(|| Hold::UsageUnknown {
                label: account.label.clone(),
            })?;
        if weekly.used_percent < 100.0 {
            return Err(Hold::Headroom {
                label: account.label.clone(),
            });
        }
    }
    if count == 0 {
        return Err(Hold::NoAccounts);
    }
    Ok(count)
}

/// The rule, without I/O: redeem nothing unless every Codex account is out of
/// weekly usage; then pick the unreserved account with an available,
/// money-free credit whose weekly window resets farthest away. Ties go to the
/// account whose credit expires soonest, then to the label. Unknown usage or
/// reset times mean no reset; one account's money-mentioning offer refuses
/// the whole decision.
pub fn decide(
    provider: Provider,
    settings: &Auto,
    entries: &[(Account, Cached)],
    log: &ResetLog,
    at: u64,
) -> std::result::Result<ResetDecision, Refusal> {
    if settings.auto_reset == AutoReset::Off {
        return Err(Hold::Off.into());
    }
    if provider != Provider::Codex {
        return Err(Hold::Unsupported.into());
    }
    if log.halted.is_some() {
        return Err(Hold::Halted.into());
    }
    if let Some(until) = log.cooldown_until().filter(|until| *until > at) {
        return Err(Hold::Cooldown { until }.into());
    }
    let accounts = guard(entries, at)?;
    let mut best: Option<(u64, u64, &str, &ResetCredit)> = None;
    for (account, cache) in entries
        .iter()
        .filter(|(a, _)| a.provider == Provider::Codex)
    {
        if !settings.allows(&account.label, None) || log.skipped(&account.label, at) {
            continue;
        }
        if cache.usage.available_usage_resets.unwrap_or(0) == 0 {
            continue;
        }
        let credits: Vec<&ResetCredit> = cache
            .usage
            .reset_credits
            .iter()
            .filter(|c| c.available() && c.expires_at.is_none_or(|e| e > at))
            .collect();
        if credits.iter().any(|c| credit_mentions_money(c)) {
            return Err(Refusal::Money {
                label: account.label.clone(),
            });
        }
        let soonest = |c: &&ResetCredit| (c.expires_at.unwrap_or(u64::MAX), c.id.clone());
        let Some(credit) = credits
            .into_iter()
            .min_by(|a, b| soonest(a).cmp(&soonest(b)))
        else {
            continue;
        };
        // guard() proved a weekly window with a future reset.
        let weekly = cache.usage.weekly_reset(at).unwrap_or(0);
        let expires = credit.expires_at.unwrap_or(u64::MAX);
        let better = best.is_none_or(|(w, e, label, _)| {
            (std::cmp::Reverse(weekly), expires, account.label.as_str())
                < (std::cmp::Reverse(w), e, label)
        });
        if better {
            best = Some((weekly, expires, account.label.as_str(), credit));
        }
    }
    let (weekly, _, label, credit) = best.ok_or(Hold::NoCredit)?;
    Ok(ResetDecision {
        label: label.to_owned(),
        credit_id: credit.id.clone(),
        weekly_resets_at: weekly,
        credit_expires_at: credit.expires_at,
        reason: format!(
            "all {accounts} Codex accounts are out of weekly usage; {label}'s weekly window resets farthest away, in {}",
            crate::display::duration(weekly.saturating_sub(at))
        ),
    })
}

/// Why `decide` returned no decision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    Hold(Hold),
    /// An available credit's text mentions money.
    Money {
        label: String,
    },
}
impl From<Hold> for Refusal {
    fn from(hold: Hold) -> Self {
        Self::Hold(hold)
    }
}

/// The I/O a reset step needs.
pub trait Port {
    /// Re-reads one account's usage, bypassing the cache.
    fn refresh(&mut self, account: &Account) -> Result<Cached>;
    /// Sends `account/rateLimitResetCredit/consume` and returns its result.
    fn redeem(
        &mut self,
        account: &Account,
        idempotency_key: &str,
        credit_id: &str,
    ) -> Result<Value>;
    fn save(&mut self, log: &ResetLog) -> Result<()>;
    fn new_key(&mut self) -> Result<String>;
}

/// What one step did.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "step", rename_all = "kebab-case")]
pub enum Step {
    Held {
        hold: Hold,
    },
    /// Dry-run: what `on` would do. `logged` is false when the same decision
    /// was already logged.
    WouldReset {
        decision: ResetDecision,
        logged: bool,
    },
    Reset {
        decision: ResetDecision,
        outcome: String,
        available_resets: Option<u64>,
    },
    /// Codex declined (nothing to reset, or no credit); not retried this window.
    Declined {
        label: String,
        outcome: String,
    },
    Failed {
        label: String,
        attempts: u32,
        retry_at: Option<u64>,
    },
    /// A redemption is waiting to be retried with its original key.
    Waiting {
        label: String,
        retry_at: u64,
    },
    /// A credit or response mentioned money.
    Refused {
        label: String,
        halted: bool,
    },
}
impl Step {
    /// One log line, or nothing for a quiet step. Never includes keys or ids.
    pub fn message(&self, at: u64) -> Option<String> {
        let dur = |t: u64| crate::display::duration(t.saturating_sub(at));
        Some(match self {
            Self::Held { .. } | Self::Waiting { .. } => return None,
            Self::WouldReset { logged: false, .. } => return None,
            Self::WouldReset { decision, .. } => format!(
                "aswap: dry-run: would reset codex:{} (weekly resets in {}; all Codex accounts exhausted)",
                decision.label,
                dur(decision.weekly_resets_at)
            ),
            Self::Reset {
                decision,
                available_resets,
                ..
            } => format!(
                "aswap: reset codex:{} usage with one reset credit (weekly was due in {}; all Codex accounts exhausted); {} left",
                decision.label,
                dur(decision.weekly_resets_at),
                available_resets.map_or("unknown".into(), |n| n.to_string())
            ),
            Self::Declined { label, outcome } => format!(
                "aswap: Codex declined the codex:{label} usage reset ({outcome}); not retrying before its weekly reset"
            ),
            Self::Failed {
                label,
                attempts,
                retry_at: Some(retry_at),
            } => format!(
                "aswap: codex:{label} usage reset failed (attempt {attempts}/{MAX_ATTEMPTS}); retrying in {}",
                dur(*retry_at)
            ),
            Self::Failed { label, .. } => format!(
                "aswap: codex:{label} usage reset failed {MAX_ATTEMPTS} times; giving up until its weekly reset"
            ),
            Self::Refused { label, halted } => format!(
                "aswap: refused to reset codex:{label}: the reset offer mentions payment, credits or upgrades{}",
                if *halted {
                    "; usage resets halted until `aswap auto codex reset off`"
                } else {
                    ""
                }
            ),
        })
    }
}

/// Runs one reset decision. Dry-run never redeems; `on` redeems at most one
/// credit per step, persisting the idempotency key before the request.
pub fn step(
    port: &mut impl Port,
    provider: Provider,
    settings: &Auto,
    mut entries: Vec<(Account, Cached)>,
    log: &mut ResetLog,
    follower: bool,
    at: u64,
) -> Result<Step> {
    let held = |hold| Ok(Step::Held { hold });
    if settings.auto_reset == AutoReset::Off {
        return held(Hold::Off);
    }
    if provider != Provider::Codex {
        return held(Hold::Unsupported);
    }
    if follower {
        return held(Hold::Follower);
    }
    log.prune(at);
    if let Some(pending) = log.pending.clone() {
        return resume(port, settings, &entries, log, pending, at);
    }
    let decision = match decide(provider, settings, &entries, log, at) {
        Ok(decision) => decision,
        Err(Refusal::Hold(hold)) => return held(hold),
        Err(Refusal::Money { label }) => return refuse(port, settings, log, label, None, at),
    };
    if settings.auto_reset == AutoReset::DryRun {
        let mark = DryRunMark {
            label: decision.label.clone(),
            weekly_resets_at: decision.weekly_resets_at,
        };
        let logged = log.last_dry_run.as_ref() != Some(&mark);
        if logged {
            log.last_dry_run = Some(mark);
            log.push(Record {
                at,
                label: decision.label.clone(),
                outcome: "dry-run".into(),
                weekly_resets_at: Some(decision.weekly_resets_at),
                reason: decision.reason.clone(),
                pre: entries
                    .iter()
                    .find(|(a, _)| a.label == decision.label)
                    .map(|(_, c)| Snapshot::of(c)),
                post: None,
            });
            port.save(log)?;
        }
        return Ok(Step::WouldReset { decision, logged });
    }
    // On: confirm with a fresh read of the chosen account before spending.
    let Some(index) = entries.iter().position(|(a, _)| a.label == decision.label) else {
        return held(Hold::Changed {
            label: decision.label,
        });
    };
    let account = entries[index].0.clone();
    let Ok(pre) = port.refresh(&account) else {
        return held(Hold::UsageUnknown {
            label: account.label,
        });
    };
    entries[index].1 = pre.clone();
    let decision = match decide(provider, settings, &entries, log, at) {
        Ok(fresh) if fresh.label == decision.label => fresh,
        Ok(_) => {
            return held(Hold::Changed {
                label: decision.label,
            });
        }
        Err(Refusal::Hold(hold)) => return held(hold),
        Err(Refusal::Money { label }) => return refuse(port, settings, log, label, None, at),
    };
    log.pending = Some(Pending {
        label: decision.label.clone(),
        credit_id: decision.credit_id.clone(),
        idempotency_key: port.new_key()?,
        weekly_resets_at: decision.weekly_resets_at,
        reason: decision.reason.clone(),
        started_at: at,
        attempts: 0,
        retry_at: at,
        pre: Some(Snapshot::of(&pre)),
    });
    port.save(log)?;
    attempt(port, settings, log, &account, at)
}

/// A pending redemption retries with its original key, so it can never spend
/// a second credit. Before each retry it is checked like a fresh decision: the
/// account is read again, must still exist, be unreserved and out of weekly
/// usage, and its credit must not have expired or mention money. If any
/// Codex account has room again the earlier request may have worked. A
/// pending whose window passed, whose account is gone, or that can no longer
/// be sent is dropped and recorded, never left waiting forever.
fn resume(
    port: &mut impl Port,
    settings: &Auto,
    entries: &[(Account, Cached)],
    log: &mut ResetLog,
    pending: Pending,
    at: u64,
) -> Result<Step> {
    let waiting = Step::Waiting {
        label: pending.label.clone(),
        retry_at: pending.retry_at,
    };
    let Some(index) = entries.iter().position(|(a, _)| a.label == pending.label) else {
        return drop_pending(port, log, pending, "its account was removed", at);
    };
    if at >= pending.weekly_resets_at {
        return drop_pending(port, log, pending, "its weekly window has passed", at);
    }
    if !settings.allows(&pending.label, None) {
        return drop_pending(port, log, pending, "its account is reserved", at);
    }
    if !matches!(guard(entries, at), Ok(_) | Err(Hold::UsageUnknown { .. })) {
        return drop_pending(port, log, pending, "an account has weekly usage again", at);
    }
    if settings.auto_reset != AutoReset::On || at < pending.retry_at || log.halted.is_some() {
        return Ok(waiting);
    }
    let account = entries[index].0.clone();
    let Ok(fresh) = port.refresh(&account) else {
        return Ok(waiting);
    };
    let mut entries = entries.to_vec();
    entries[index].1 = fresh;
    match guard(&entries, at) {
        Ok(_) => {}
        Err(Hold::UsageUnknown { .. }) => return Ok(waiting),
        Err(_) => {
            return drop_pending(port, log, pending, "an account has weekly usage again", at);
        }
    }
    // The credit is sent by id, so only that credit can be spent. A credit
    // no longer listed may be the one this key already redeemed: retry.
    if let Some(credit) = entries[index]
        .1
        .usage
        .reset_credits
        .iter()
        .find(|c| c.id == pending.credit_id)
    {
        if credit_mentions_money(credit) {
            let label = pending.label.clone();
            log.abandon(pending, "its credit now mentions money", at);
            return refuse(port, settings, log, label, None, at);
        }
        if credit.expires_at.is_some_and(|e| e <= at) {
            return drop_pending(port, log, pending, "its credit has expired", at);
        }
    }
    attempt(port, settings, log, &account, at)
}

/// Drops a pending redemption and records why. One that was sent may have
/// spent its credit: it is recorded as unconfirmed, which starts the
/// cooldown, and its account is skipped until that weekly window passes.
fn drop_pending(
    port: &mut impl Port,
    log: &mut ResetLog,
    pending: Pending,
    why: &str,
    at: u64,
) -> Result<Step> {
    let label = pending.label.clone();
    log.abandon(pending, why, at);
    port.save(log)?;
    Ok(Step::Held {
        hold: Hold::Changed { label },
    })
}

enum Outcome {
    Reset(&'static str),
    Declined(&'static str),
    Money,
    Unknown,
}
fn classify(v: &Value) -> Outcome {
    if response_mentions_money(v) {
        return Outcome::Money;
    }
    match v.get("outcome").and_then(Value::as_str) {
        Some("reset") => Outcome::Reset("reset"),
        Some("alreadyRedeemed") => Outcome::Reset("already-redeemed"),
        Some("nothingToReset") => Outcome::Declined("nothing-to-reset"),
        Some("noCredit") => Outcome::Declined("no-credit"),
        _ => Outcome::Unknown,
    }
}

/// Sends the pending redemption once. The attempt is counted and saved first.
fn attempt(
    port: &mut impl Port,
    settings: &Auto,
    log: &mut ResetLog,
    account: &Account,
    at: u64,
) -> Result<Step> {
    let mut pending = log.pending.clone().ok_or(Error("no pending reset"))?;
    pending.attempts += 1;
    log.pending = Some(pending.clone());
    port.save(log)?;
    let result = port.redeem(account, &pending.idempotency_key, &pending.credit_id);
    let decision = ResetDecision {
        label: pending.label.clone(),
        credit_id: pending.credit_id.clone(),
        weekly_resets_at: pending.weekly_resets_at,
        credit_expires_at: None,
        reason: pending.reason.clone(),
    };
    let mut record = Record {
        at,
        label: pending.label.clone(),
        outcome: String::new(),
        weekly_resets_at: Some(pending.weekly_resets_at),
        reason: pending.reason.clone(),
        pre: pending.pre.clone(),
        post: None,
    };
    match result.as_ref().map(classify).unwrap_or(Outcome::Unknown) {
        Outcome::Money => {
            log.pending = None;
            let label = pending.label.clone();
            refuse(port, settings, log, label, Some(record), at)
        }
        Outcome::Reset(outcome) => {
            log.pending = None;
            let post = port.refresh(account).ok();
            let available_resets = post.as_ref().and_then(|c| c.usage.available_usage_resets);
            record.outcome = outcome.into();
            record.post = post.as_ref().map(Snapshot::of);
            log.push(record);
            port.save(log)?;
            Ok(Step::Reset {
                decision,
                outcome: outcome.into(),
                available_resets,
            })
        }
        Outcome::Declined(outcome) => {
            log.pending = None;
            record.outcome = outcome.into();
            log.push(record);
            log.skip(&pending.label, pending.weekly_resets_at);
            port.save(log)?;
            Ok(Step::Declined {
                label: pending.label,
                outcome: outcome.into(),
            })
        }
        Outcome::Unknown if pending.attempts >= MAX_ATTEMPTS => {
            log.pending = None;
            record.outcome = "gave-up".into();
            log.push(record);
            log.skip(&pending.label, pending.weekly_resets_at);
            port.save(log)?;
            Ok(Step::Failed {
                label: pending.label,
                attempts: pending.attempts,
                retry_at: None,
            })
        }
        Outcome::Unknown => {
            let retry_at = at.saturating_add(RETRY_BASE << (pending.attempts - 1).min(8));
            if let Some(p) = log.pending.as_mut() {
                p.retry_at = retry_at;
            }
            record.outcome = "failed".into();
            log.push(record);
            port.save(log)?;
            Ok(Step::Failed {
                label: pending.label,
                attempts: pending.attempts,
                retry_at: Some(retry_at),
            })
        }
    }
}

/// Money wording: in `on`, halt until `reset off`; in dry-run, only report.
/// `response` is the attempt's record when a redeem response mentioned money.
fn refuse(
    port: &mut impl Port,
    settings: &Auto,
    log: &mut ResetLog,
    label: String,
    response: Option<Record>,
    at: u64,
) -> Result<Step> {
    let halted = settings.auto_reset == AutoReset::On;
    let repeated = log
        .history
        .last()
        .is_some_and(|r| r.outcome == "refused" && r.label == label);
    if halted || !repeated || response.is_some() {
        let outcome = if response.is_some() {
            "refused-response"
        } else {
            "refused"
        };
        let mut record = response.unwrap_or(Record {
            at,
            label: label.clone(),
            outcome: String::new(),
            weekly_resets_at: None,
            reason: String::new(),
            pre: None,
            post: None,
        });
        record.outcome = outcome.into();
        record.reason = "the reset offer mentions payment, credits or upgrades".into();
        log.push(record);
        if halted {
            log.halted = Some(format!("codex:{label} reset offer mentioned money"));
        }
        port.save(log)?;
    }
    Ok(Step::Refused { label, halted })
}

pub fn log_path(store: &Store) -> PathBuf {
    store.root.join("codex-resets.json")
}
/// Caller holds the store lock.
pub fn load(store: &Store) -> Result<ResetLog> {
    let path = log_path(store);
    if path.exists() {
        store::json_read(&path)
    } else {
        Ok(ResetLog::default())
    }
}
/// Sets one provider's reset mode. Only Codex has a reset log: `codex reset
/// off` clears its halt and drops (and records) a pending redemption; other
/// modes, and every Claude setting, leave the log alone. Caller holds the
/// store lock.
pub fn set_mode(
    store: &Store,
    provider: Provider,
    settings: &mut Auto,
    mode: AutoReset,
) -> Result<()> {
    if provider != Provider::Codex && mode != AutoReset::Off {
        return Err(Error("usage-limit resets are only supported for Codex"));
    }
    settings.auto_reset = mode;
    if provider == Provider::Codex && mode == AutoReset::Off {
        let mut log = load(store)?;
        if log.turn_off(crate::now()) {
            store::json_write(&log_path(store), &log)?;
        }
    }
    Ok(())
}

struct StorePort<'a> {
    store: &'a Store,
}
impl Port for StorePort<'_> {
    fn refresh(&mut self, account: &Account) -> Result<Cached> {
        usage::cached_force(self.store, account, true)
    }
    fn redeem(
        &mut self,
        account: &Account,
        idempotency_key: &str,
        credit_id: &str,
    ) -> Result<Value> {
        let _ = crate::refresh::refresh(self.store, account, crate::now(), false);
        let home = {
            let _lock = self.store.lock()?;
            let profile = self.store.profile(account.provider, &account.label)?;
            usage::access_only_home(account, &profile)?
        };
        let diagnostic = std::cell::Cell::new(usage::Failure::new("codex-rpc-unavailable"));
        usage::codex_request_within(
            home.path(),
            &diagnostic,
            &[],
            METHOD,
            serde_json::json!({"idempotencyKey": idempotency_key, "creditId": credit_id}),
            RPC_TIMEOUT,
        )
    }
    fn save(&mut self, log: &ResetLog) -> Result<()> {
        let _lock = self.store.lock()?;
        store::json_write(&log_path(self.store), log)
    }
    fn new_key(&mut self) -> Result<String> {
        use ring::rand::SecureRandom;
        let mut bytes = [0u8; 16];
        ring::rand::SystemRandom::new()
            .fill(&mut bytes)
            .safe("cannot create a reset idempotency key")?;
        bytes[6] = (bytes[6] & 15) | 64;
        bytes[8] = (bytes[8] & 63) | 128;
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        Ok(format!(
            "{}-{}-{}-{}-{}",
            &hex[..8],
            &hex[8..12],
            &hex[12..16],
            &hex[16..20],
            &hex[20..]
        ))
    }
}

/// One monitor tick: reads every Codex account's usage through the shared
/// cache and runs one step. Off reads nothing. A second process skips the
/// tick while another holds the reset lock.
pub fn tick(store: &Store, provider: Provider) -> Result<Option<Step>> {
    let settings = {
        let _lock = store.lock()?;
        store
            .load()?
            .auto
            .get(&provider)
            .cloned()
            .unwrap_or_default()
    };
    if settings.auto_reset == AutoReset::Off || provider != Provider::Codex {
        return Ok(None);
    }
    let Some(_exclusive) = exclusive(store)? else {
        return Ok(None);
    };
    let follower = crate::coordination::follower(store)?;
    let (entries, settings) = crate::runner::observations(store, provider)?;
    let mut log = {
        let _lock = store.lock()?;
        load(store)?
    };
    step(
        &mut StorePort { store },
        provider,
        &settings,
        entries,
        &mut log,
        follower,
        crate::now(),
    )
    .map(Some)
}
fn exclusive(store: &Store) -> Result<Option<std::fs::File>> {
    use fs2::FileExt;
    use std::os::unix::fs::OpenOptionsExt;
    let path = store.root.join("codex-reset.lock");
    store::no_links(&path)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .safe("cannot open reset lock")?;
    Ok(file.try_lock_exclusive().is_ok().then_some(file))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::{Usage, Window};

    const AT: u64 = 1_800_000_000;
    const DAY: u64 = 86_400;

    fn codex(label: &str) -> Account {
        Account {
            provider: Provider::Codex,
            label: label.into(),
        }
    }
    fn credit(id: &str, expires_at: Option<u64>) -> ResetCredit {
        ResetCredit {
            id: id.into(),
            status: "available".into(),
            reset_type: "codexRateLimits".into(),
            expires_at,
            title: Some("Full reset (Weekly + 5 hr)".into()),
            description: Some("Ready to redeem".into()),
        }
    }
    /// A fresh Codex reading: weekly at `weekly`% resetting in `days`.
    fn cache(weekly: f64, days: Option<u64>, credits: Vec<ResetCredit>) -> Cached {
        Cached {
            usage: Usage {
                windows: vec![
                    Window {
                        name: "session".into(),
                        used_percent: 12.0,
                        resets_at: Some(AT + 3600),
                        duration_minutes: Some(300),
                        codex_slot: Some(0),
                    },
                    Window {
                        name: "weekly".into(),
                        used_percent: weekly,
                        resets_at: days.map(|d| AT + d * DAY),
                        duration_minutes: Some(10080),
                        codex_slot: Some(1),
                    },
                ],
                available_usage_resets: Some(credits.len() as u64),
                reset_credits: credits,
                ..Default::default()
            },
            fetched_at: AT,
            next_poll_at: AT + 180,
            last_attempt: AT,
            ..Default::default()
        }
    }
    fn settings(mode: AutoReset) -> Auto {
        Auto {
            auto_reset: mode,
            ..Default::default()
        }
    }
    fn exhausted() -> Vec<(Account, Cached)> {
        vec![
            (
                codex("three"),
                cache(100.0, Some(3), vec![credit("c3", None)]),
            ),
            (
                codex("six"),
                cache(100.0, Some(6), vec![credit("c6", None)]),
            ),
            (
                codex("one"),
                cache(100.0, Some(1), vec![credit("c1", None)]),
            ),
        ]
    }
    fn pick(entries: &[(Account, Cached)]) -> std::result::Result<ResetDecision, Refusal> {
        decide(
            Provider::Codex,
            &settings(AutoReset::On),
            entries,
            &ResetLog::default(),
            AT,
        )
    }

    #[test]
    fn all_exhausted_resets_the_account_whose_weekly_window_resets_farthest_away() {
        let decision = pick(&exhausted()).unwrap();
        assert_eq!(decision.label, "six");
        assert_eq!(decision.credit_id, "c6");
        assert_eq!(decision.weekly_resets_at, AT + 6 * DAY);
        assert!(decision.reason.contains("all 3 Codex accounts"));
        // Entry order does not matter.
        let mut reversed = exhausted();
        reversed.reverse();
        assert_eq!(pick(&reversed).unwrap().label, "six");
    }

    #[test]
    fn any_account_with_weekly_headroom_means_no_reset() {
        for (index, used) in [(0, 99.9), (1, 0.0), (2, 50.0)] {
            let mut entries = exhausted();
            entries[index].1 = cache(used, Some(2), vec![credit("x", None)]);
            assert_eq!(
                pick(&entries),
                Err(Hold::Headroom {
                    label: entries[index].0.label.clone()
                }
                .into())
            );
        }
        // Headroom on a reserved account still blocks: it is a Codex account with room.
        let mut entries = exhausted();
        entries[2].1 = cache(10.0, Some(1), vec![]);
        let mut reserved = settings(AutoReset::On);
        reserved.reserved = vec!["one".into()];
        assert!(matches!(
            decide(
                Provider::Codex,
                &reserved,
                &entries,
                &ResetLog::default(),
                AT
            ),
            Err(Refusal::Hold(Hold::Headroom { .. }))
        ));
    }

    #[test]
    fn a_full_session_window_with_weekly_headroom_is_not_out_of_weekly_usage() {
        let mut entries = exhausted();
        entries[0].1.usage.windows[0].used_percent = 100.0;
        entries[0].1.usage.windows[1].used_percent = 70.0;
        assert!(matches!(
            pick(&entries),
            Err(Refusal::Hold(Hold::Headroom { .. }))
        ));
    }

    #[test]
    fn unknown_usage_or_reset_time_means_no_reset() {
        let unknown = |change: &dyn Fn(&mut Cached)| {
            let mut entries = exhausted();
            change(&mut entries[0].1);
            pick(&entries)
        };
        let expected = Err(Hold::UsageUnknown {
            label: "three".into(),
        }
        .into());
        // No weekly reset time.
        assert_eq!(unknown(&|c| c.usage.windows[1].resets_at = None), expected);
        // No weekly window at all.
        assert_eq!(
            unknown(&|c| {
                c.usage.windows.remove(1);
            }),
            expected
        );
        // Stale reading, failed reading, and a passed reset.
        assert_eq!(unknown(&|c| c.fetched_at = AT - 601), expected);
        assert_eq!(unknown(&|c| c.error = Some("http-429".into())), expected);
        assert_eq!(
            unknown(&|c| c.usage.windows[0].resets_at = Some(AT)),
            expected
        );
        // An account never read.
        assert_eq!(unknown(&|c| *c = Cached::default()), expected);
    }

    #[test]
    fn ties_break_by_soonest_credit_expiry_then_label() {
        let entries = vec![
            (
                codex("b"),
                cache(100.0, Some(6), vec![credit("b1", Some(AT + 9 * DAY))]),
            ),
            (codex("a"), cache(100.0, Some(6), vec![credit("a1", None)])),
            (
                codex("c"),
                cache(100.0, Some(6), vec![credit("c1", Some(AT + 2 * DAY))]),
            ),
        ];
        assert_eq!(pick(&entries).unwrap().label, "c");
        let entries = vec![
            (codex("b"), cache(100.0, Some(6), vec![credit("b1", None)])),
            (codex("a"), cache(100.0, Some(6), vec![credit("a1", None)])),
        ];
        assert_eq!(pick(&entries).unwrap().label, "a");
        // Within an account the soonest-expiring credit goes first.
        let entries = vec![(
            codex("a"),
            cache(
                100.0,
                Some(6),
                vec![
                    credit("late", Some(AT + 20 * DAY)),
                    credit("soon", Some(AT + DAY)),
                ],
            ),
        )];
        assert_eq!(pick(&entries).unwrap().credit_id, "soon");
    }

    #[test]
    fn no_allowance_means_no_reset() {
        let mut entries = exhausted();
        for (_, c) in &mut entries {
            c.usage.available_usage_resets = Some(0);
        }
        assert_eq!(pick(&entries), Err(Hold::NoCredit.into()));
        // Count unknown, or a count without an available credit row.
        let mut entries = exhausted();
        for (_, c) in &mut entries {
            c.usage.available_usage_resets = None;
        }
        assert_eq!(pick(&entries), Err(Hold::NoCredit.into()));
        let mut entries = exhausted();
        for (_, c) in &mut entries {
            c.usage.reset_credits.clear();
        }
        assert_eq!(pick(&entries), Err(Hold::NoCredit.into()));
        // Redeemed, redeeming, expired and unknown-type credits are not available.
        let mut entries = exhausted();
        entries.truncate(1);
        let credits = &mut entries[0].1.usage.reset_credits;
        credits[0].status = "redeemed".into();
        credits.push(ResetCredit {
            status: "redeeming".into(),
            ..credit("r", None)
        });
        credits.push(credit("expired", Some(AT)));
        credits.push(ResetCredit {
            reset_type: "unknown".into(),
            ..credit("u", None)
        });
        assert_eq!(pick(&entries), Err(Hold::NoCredit.into()));
        // The farthest account without a credit falls through to the next.
        let mut entries = exhausted();
        entries[1].1.usage.available_usage_resets = Some(0);
        assert_eq!(pick(&entries).unwrap().label, "three");
    }

    #[test]
    fn reserved_and_skipped_accounts_are_not_reset() {
        let mut reserved = settings(AutoReset::On);
        reserved.reserved = vec!["six".into()];
        let entries = exhausted();
        assert_eq!(
            decide(
                Provider::Codex,
                &reserved,
                &entries,
                &ResetLog::default(),
                AT
            )
            .unwrap()
            .label,
            "three"
        );
        let mut log = ResetLog::default();
        log.skip("six", AT + 6 * DAY);
        assert_eq!(
            decide(
                Provider::Codex,
                &settings(AutoReset::On),
                &entries,
                &log,
                AT
            )
            .unwrap()
            .label,
            "three"
        );
    }

    #[test]
    fn off_claude_halted_and_cooldown_never_decide() {
        let entries = exhausted();
        let log = ResetLog::default();
        assert_eq!(
            decide(
                Provider::Codex,
                &settings(AutoReset::Off),
                &entries,
                &log,
                AT
            ),
            Err(Hold::Off.into())
        );
        assert!(
            decide(
                Provider::Codex,
                &settings(AutoReset::DryRun),
                &entries,
                &log,
                AT
            )
            .is_ok()
        );
        let claude: Vec<_> = entries
            .iter()
            .map(|(a, c)| {
                (
                    Account {
                        provider: Provider::Claude,
                        label: a.label.clone(),
                    },
                    c.clone(),
                )
            })
            .collect();
        assert_eq!(
            decide(
                Provider::Claude,
                &settings(AutoReset::On),
                &claude,
                &log,
                AT
            ),
            Err(Hold::Unsupported.into())
        );
        // Claude entries never count as Codex accounts.
        assert_eq!(
            decide(Provider::Codex, &settings(AutoReset::On), &claude, &log, AT),
            Err(Hold::NoAccounts.into())
        );
        let halted = ResetLog {
            halted: Some("synthetic".into()),
            ..Default::default()
        };
        assert_eq!(
            decide(
                Provider::Codex,
                &settings(AutoReset::On),
                &entries,
                &halted,
                AT
            ),
            Err(Hold::Halted.into())
        );
        let mut recent = ResetLog::default();
        recent.push(Record {
            at: AT - 10,
            label: "six".into(),
            outcome: "reset".into(),
            weekly_resets_at: None,
            reason: String::new(),
            pre: None,
            post: None,
        });
        assert_eq!(
            decide(
                Provider::Codex,
                &settings(AutoReset::On),
                &entries,
                &recent,
                AT
            ),
            Err(Hold::Cooldown {
                until: AT - 10 + COOLDOWN
            }
            .into())
        );
        // The cooldown ends exactly one hour after the attempt.
        recent.history[0].at = AT - COOLDOWN;
        assert!(
            decide(
                Provider::Codex,
                &settings(AutoReset::On),
                &entries,
                &recent,
                AT
            )
            .is_ok()
        );
    }

    #[test]
    fn money_wording_refuses_and_plain_reset_wording_passes() {
        for text in [
            "Buy more usage",
            "Purchase a reset",
            "Uses 5 credits",
            "Add credits to continue",
            "add-credits",
            "Upgrade to Pro",
            "Billing: $5",
            "Extra usage",
            "Pay to reset",
            "Charged to your card",
            "Subscribe for more",
            "€4",
        ] {
            assert!(mentions_money(text), "{text}");
        }
        for text in [
            "Full reset (Weekly + 5 hr)",
            "Ready to redeem",
            "Reset your current usage limits",
            "Expires 3 Dec 2026",
        ] {
            assert!(!mentions_money(text), "{text}");
        }
        let mut entries = exhausted();
        entries[0].1.usage.reset_credits[0].description = Some("Purchase more usage".into());
        assert_eq!(
            pick(&entries),
            Err(Refusal::Money {
                label: "three".into()
            })
        );
        let mut entries = exhausted();
        entries[2].1.usage.reset_credits[0].title = Some("Upgrade for resets".into());
        assert!(matches!(pick(&entries), Err(Refusal::Money { .. })));
    }

    #[derive(Default)]
    struct Fake {
        refreshed: Vec<String>,
        /// Each redeem call: (label, key, credit id), with the log as saved before it.
        redeemed: Vec<(String, String, String, Option<Pending>)>,
        responses: Vec<Result<Value>>,
        after: Option<Cached>,
        before: Option<Cached>,
        saved: Vec<ResetLog>,
        keys: u32,
        /// When set, a fresh read is stamped with this time.
        clock: Option<u64>,
    }
    impl Port for Fake {
        /// `before` until a redeem call, then `after` if given.
        fn refresh(&mut self, account: &Account) -> Result<Cached> {
            self.refreshed.push(account.label.clone());
            let next = match &self.after {
                Some(after) if !self.redeemed.is_empty() => Some(after.clone()),
                _ => self.before.clone(),
            };
            let mut next = next.ok_or(Error("synthetic refresh failure"))?;
            if let Some(clock) = self.clock {
                next.fetched_at = clock;
            }
            Ok(next)
        }
        fn redeem(&mut self, account: &Account, key: &str, credit: &str) -> Result<Value> {
            let persisted = self.saved.last().and_then(|log| log.pending.clone());
            self.redeemed
                .push((account.label.clone(), key.into(), credit.into(), persisted));
            if self.responses.is_empty() {
                return Err(Error("synthetic timeout"));
            }
            self.responses.remove(0)
        }
        fn save(&mut self, log: &ResetLog) -> Result<()> {
            self.saved.push(log.clone());
            Ok(())
        }
        fn new_key(&mut self) -> Result<String> {
            self.keys += 1;
            Ok(format!("synthetic-key-{}", self.keys))
        }
    }
    /// One step over `exhausted()`, read just now.
    fn run(fake: &mut Fake, mode: AutoReset, log: &mut ResetLog, at: u64) -> Step {
        fake.clock = Some(at);
        let mut entries = exhausted();
        for (_, c) in &mut entries {
            c.fetched_at = at;
        }
        step(
            fake,
            Provider::Codex,
            &settings(mode),
            entries,
            log,
            false,
            at,
        )
        .unwrap()
    }

    #[test]
    fn dry_run_only_logs_what_it_would_reset_and_never_redeems() {
        let mut fake = Fake::default();
        let mut log = ResetLog::default();
        let first = run(&mut fake, AutoReset::DryRun, &mut log, AT);
        let Step::WouldReset { decision, logged } = &first else {
            panic!("{first:?}");
        };
        assert!(*logged);
        assert_eq!(decision.label, "six");
        assert_eq!(
            first.message(AT).unwrap(),
            "aswap: dry-run: would reset codex:six (weekly resets in 6d0h; all Codex accounts exhausted)"
        );
        // The same decision is not logged again every tick.
        let again = run(&mut fake, AutoReset::DryRun, &mut log, AT + 180);
        assert!(matches!(again, Step::WouldReset { logged: false, .. }));
        assert!(again.message(AT + 180).is_none());
        assert!(fake.redeemed.is_empty() && fake.refreshed.is_empty());
        assert_eq!(log.history.len(), 1);
        assert_eq!(log.history[0].outcome, "dry-run");
        assert!(log.pending.is_none());
        // Off does nothing at all.
        let mut off = Fake::default();
        assert_eq!(
            run(&mut off, AutoReset::Off, &mut ResetLog::default(), AT),
            Step::Held { hold: Hold::Off }
        );
        assert!(off.saved.is_empty() && off.refreshed.is_empty());
    }

    #[test]
    fn on_persists_the_key_then_redeems_exactly_once_and_records_pre_and_post_usage() {
        let mut fake = Fake {
            before: Some(cache(100.0, Some(6), vec![credit("c6", None)])),
            after: Some(cache(0.0, Some(7), vec![])),
            responses: vec![Ok(serde_json::json!({"outcome": "reset"}))],
            ..Default::default()
        };
        let mut log = ResetLog::default();
        let done = run(&mut fake, AutoReset::On, &mut log, AT);
        assert!(
            matches!(&done, Step::Reset { decision, outcome, .. } if decision.label == "six" && outcome == "reset")
        );
        assert_eq!(fake.redeemed.len(), 1);
        let (label, key, credit_id, persisted) = &fake.redeemed[0];
        assert_eq!((label.as_str(), credit_id.as_str()), ("six", "c6"));
        let persisted = persisted.as_ref().expect("key persisted before the call");
        assert_eq!(&persisted.idempotency_key, key);
        assert_eq!(persisted.attempts, 1);
        // Pre-read before spending, post-read after.
        assert_eq!(fake.refreshed, ["six", "six"]);
        let record = log.history.last().unwrap();
        assert_eq!(record.outcome, "reset");
        assert_eq!(record.pre.as_ref().unwrap().windows[1].used_percent, 100.0);
        assert_eq!(record.post.as_ref().unwrap().windows[1].used_percent, 0.0);
        assert!(log.pending.is_none());
        let message = done.message(AT).unwrap();
        assert!(message.contains("reset codex:six") && !message.contains("synthetic-key"));
        // The cooldown stops a second reset even if the cache still looks exhausted.
        let next = run(&mut fake, AutoReset::On, &mut log, AT + 180);
        assert!(matches!(
            next,
            Step::Held {
                hold: Hold::Cooldown { .. }
            }
        ));
        assert_eq!(fake.redeemed.len(), 1);
        // The serialized log never carries the key outside `pending`.
        let summary = log.summary().to_string();
        assert!(!summary.contains("synthetic-key") && !summary.contains("c6"));
    }

    #[test]
    fn on_rechecks_fresh_usage_before_spending() {
        // The fresh read shows the account has room again: nothing is redeemed.
        let mut fake = Fake {
            before: Some(cache(40.0, Some(6), vec![credit("c6", None)])),
            ..Default::default()
        };
        let mut log = ResetLog::default();
        assert_eq!(
            run(&mut fake, AutoReset::On, &mut log, AT),
            Step::Held {
                hold: Hold::Headroom {
                    label: "six".into()
                }
            }
        );
        // The fresh read fails: nothing is redeemed either.
        let mut failing = Fake::default();
        assert!(matches!(
            run(&mut failing, AutoReset::On, &mut log, AT),
            Step::Held {
                hold: Hold::UsageUnknown { .. }
            }
        ));
        assert!(fake.redeemed.is_empty() && failing.redeemed.is_empty());
        assert!(log.pending.is_none());
    }

    #[test]
    fn failures_back_off_reuse_the_key_and_give_up_until_the_next_window() {
        let mut fake = Fake {
            before: Some(cache(100.0, Some(6), vec![credit("c6", None)])),
            ..Default::default()
        };
        let mut log = ResetLog::default();
        let first = run(&mut fake, AutoReset::On, &mut log, AT);
        assert_eq!(
            first,
            Step::Failed {
                label: "six".into(),
                attempts: 1,
                retry_at: Some(AT + RETRY_BASE)
            }
        );
        // No retry before the backoff, and no new decision meanwhile.
        assert!(matches!(
            run(&mut fake, AutoReset::On, &mut log, AT + 60),
            Step::Waiting { .. }
        ));
        assert_eq!(fake.redeemed.len(), 1);
        let second = run(&mut fake, AutoReset::On, &mut log, AT + RETRY_BASE);
        assert!(
            matches!(second, Step::Failed { attempts: 2, retry_at: Some(r), .. } if r == AT + 3 * RETRY_BASE)
        );
        let third = run(&mut fake, AutoReset::On, &mut log, AT + 3 * RETRY_BASE);
        assert!(matches!(
            third,
            Step::Failed {
                attempts: 3,
                retry_at: None,
                ..
            }
        ));
        assert_eq!(fake.redeemed.len(), 3);
        // Every retry reused the first key and credit.
        assert!(
            fake.redeemed
                .iter()
                .all(|(l, k, c, _)| l == "six" && k == "synthetic-key-1" && c == "c6")
        );
        assert_eq!(fake.keys, 1);
        assert!(log.pending.is_none());
        assert!(log.skipped("six", AT + 5 * DAY));
        // Gave up: the cooldown holds first, then another account is eligible.
        assert!(matches!(
            run(&mut fake, AutoReset::On, &mut log, AT + 3 * RETRY_BASE + 1),
            Step::Held {
                hold: Hold::Cooldown { .. }
            }
        ));
    }

    #[test]
    fn a_pending_reset_whose_accounts_regained_room_is_recorded_unconfirmed() {
        let mut log = ResetLog {
            pending: Some(Pending {
                label: "six".into(),
                credit_id: "c6".into(),
                idempotency_key: "synthetic-key-1".into(),
                weekly_resets_at: AT + 6 * DAY,
                reason: String::new(),
                started_at: AT - 900,
                attempts: 1,
                retry_at: AT - 1,
                pre: None,
            }),
            ..Default::default()
        };
        let mut entries = exhausted();
        entries[1].1 = cache(0.0, Some(7), vec![]);
        let mut fake = Fake::default();
        let held = step(
            &mut fake,
            Provider::Codex,
            &settings(AutoReset::On),
            entries,
            &mut log,
            false,
            AT,
        )
        .unwrap();
        assert!(matches!(
            held,
            Step::Held {
                hold: Hold::Changed { .. }
            }
        ));
        assert!(fake.redeemed.is_empty() && log.pending.is_none());
        assert_eq!(log.history.last().unwrap().outcome, "unconfirmed");
    }

    #[test]
    fn declined_outcomes_are_not_retried_this_window() {
        for (response, outcome) in [
            ("nothingToReset", "nothing-to-reset"),
            ("noCredit", "no-credit"),
        ] {
            let mut fake = Fake {
                before: Some(cache(100.0, Some(6), vec![credit("c6", None)])),
                responses: vec![Ok(serde_json::json!({ "outcome": response }))],
                ..Default::default()
            };
            let mut log = ResetLog::default();
            assert_eq!(
                run(&mut fake, AutoReset::On, &mut log, AT),
                Step::Declined {
                    label: "six".into(),
                    outcome: outcome.into()
                }
            );
            assert!(log.skipped("six", AT + DAY));
            assert!(log.pending.is_none());
            // Declined outcomes did not spend, so no cooldown: the next account is chosen.
            fake.before = Some(cache(100.0, Some(3), vec![credit("c3", None)]));
            fake.redeemed.clear();
            fake.responses = vec![Ok(serde_json::json!({ "outcome": "reset" }))];
            fake.after = Some(cache(0.0, Some(7), vec![]));
            let next = run(&mut fake, AutoReset::On, &mut log, AT + 180);
            assert!(matches!(next, Step::Reset { decision, .. } if decision.label == "three"));
        }
    }

    #[test]
    fn money_in_an_offer_or_a_response_refuses_and_halts() {
        // An offer mentioning money: no call, halted in `on`.
        let mut entries = exhausted();
        entries[1].1.usage.reset_credits[0].title = Some("Buy a reset".into());
        let mut fake = Fake::default();
        let mut log = ResetLog::default();
        let refused = step(
            &mut fake,
            Provider::Codex,
            &settings(AutoReset::On),
            entries.clone(),
            &mut log,
            false,
            AT,
        )
        .unwrap();
        assert_eq!(
            refused,
            Step::Refused {
                label: "six".into(),
                halted: true
            }
        );
        assert!(fake.redeemed.is_empty() && log.halted.is_some());
        assert!(matches!(
            run(&mut fake, AutoReset::On, &mut log, AT + 180),
            Step::Held { hold: Hold::Halted }
        ));
        // Dry-run reports the refusal once without halting.
        let mut dry = ResetLog::default();
        let refused = step(
            &mut fake,
            Provider::Codex,
            &settings(AutoReset::DryRun),
            entries,
            &mut dry,
            false,
            AT,
        )
        .unwrap();
        assert_eq!(
            refused,
            Step::Refused {
                label: "six".into(),
                halted: false
            }
        );
        assert!(dry.halted.is_none());
        // A response mentioning money halts after the call.
        let mut fake = Fake {
            before: Some(cache(100.0, Some(6), vec![credit("c6", None)])),
            responses: vec![Ok(
                serde_json::json!({"outcome": "reset", "upsell": "Purchase extra usage"}),
            )],
            ..Default::default()
        };
        let mut log = ResetLog::default();
        assert!(matches!(
            run(&mut fake, AutoReset::On, &mut log, AT),
            Step::Refused { halted: true, .. }
        ));
        assert_eq!(log.history.last().unwrap().outcome, "refused-response");
        assert!(log.pending.is_none() && log.halted.is_some());
    }

    #[test]
    fn followers_and_claude_never_step() {
        let mut fake = Fake::default();
        let mut log = ResetLog::default();
        assert_eq!(
            step(
                &mut fake,
                Provider::Codex,
                &settings(AutoReset::On),
                exhausted(),
                &mut log,
                true,
                AT
            )
            .unwrap(),
            Step::Held {
                hold: Hold::Follower
            }
        );
        assert_eq!(
            step(
                &mut fake,
                Provider::Claude,
                &settings(AutoReset::On),
                exhausted(),
                &mut log,
                false,
                AT
            )
            .unwrap(),
            Step::Held {
                hold: Hold::Unsupported
            }
        );
        assert!(fake.redeemed.is_empty() && fake.saved.is_empty());
    }

    fn pending(attempts: u32, retry_at: u64) -> Pending {
        Pending {
            label: "six".into(),
            credit_id: "c6".into(),
            idempotency_key: "synthetic-key-1".into(),
            weekly_resets_at: AT + 6 * DAY,
            reason: "synthetic".into(),
            started_at: AT - 900,
            attempts,
            retry_at,
            pre: None,
        }
    }
    fn resume_with(
        fake: &mut Fake,
        log: &mut ResetLog,
        entries: Vec<(Account, Cached)>,
        at: u64,
    ) -> Step {
        fake.clock = Some(at);
        step(
            fake,
            Provider::Codex,
            &settings(AutoReset::On),
            entries,
            log,
            false,
            at,
        )
        .unwrap()
    }
    fn exhausted_at(at: u64) -> Vec<(Account, Cached)> {
        let mut entries = exhausted();
        for (_, c) in &mut entries {
            c.fetched_at = at;
        }
        entries
    }

    #[test]
    fn a_crashed_attempt_resends_its_saved_key_and_records_already_redeemed() {
        // Saved before sending, then the monitor died: attempts is still 0.
        let mut log = ResetLog {
            pending: Some(pending(0, AT)),
            ..Default::default()
        };
        let mut fake = Fake {
            before: Some(cache(100.0, Some(6), vec![credit("c6", None)])),
            after: Some(cache(0.0, Some(7), vec![])),
            responses: vec![Ok(serde_json::json!({"outcome": "alreadyRedeemed"}))],
            ..Default::default()
        };
        let done = resume_with(&mut fake, &mut log, exhausted_at(AT), AT);
        assert!(matches!(&done, Step::Reset { outcome, .. } if outcome == "already-redeemed"));
        assert_eq!(fake.keys, 0, "no new key");
        assert_eq!(fake.redeemed.len(), 1);
        assert_eq!(fake.redeemed[0].1, "synthetic-key-1");
        // A fresh read before resending, and one after.
        assert_eq!(fake.refreshed, ["six", "six"]);
        assert_eq!(log.history.last().unwrap().outcome, "already-redeemed");
        assert!(log.pending.is_none() && log.cooldown_until().is_some());
    }

    #[test]
    fn a_pending_is_dropped_and_recorded_instead_of_waiting_forever() {
        type Change = fn(&mut Vec<(Account, Cached)>, &mut Auto) -> u64;
        let cases: [(&str, Change); 3] = [
            ("its account was removed", |entries, _| {
                entries.retain(|(a, _)| a.label != "six");
                AT
            }),
            ("its weekly window has passed", |_, _| AT + 6 * DAY),
            ("its account is reserved", |_, settings| {
                settings.reserved = vec!["six".into()];
                AT
            }),
        ];
        for (why, change) in cases {
            for attempts in [0, 2] {
                let mut entries = exhausted_at(AT);
                let mut settings = settings(AutoReset::On);
                let at = change(&mut entries, &mut settings);
                for (_, c) in &mut entries {
                    c.fetched_at = at;
                    // Keep later readings valid past the session reset.
                    c.usage.windows[0].resets_at = Some(at + 3600);
                    c.usage.windows[1].resets_at = Some(at + DAY);
                }
                let mut log = ResetLog {
                    pending: Some(pending(attempts, AT)),
                    ..Default::default()
                };
                let mut fake = Fake::default();
                let held = step(
                    &mut fake,
                    Provider::Codex,
                    &settings,
                    entries,
                    &mut log,
                    false,
                    at,
                )
                .unwrap();
                assert!(
                    matches!(
                        held,
                        Step::Held {
                            hold: Hold::Changed { .. }
                        }
                    ),
                    "{why}"
                );
                assert!(
                    fake.redeemed.is_empty() && fake.refreshed.is_empty(),
                    "{why}"
                );
                assert!(log.pending.is_none(), "{why}");
                let record = log.history.last().unwrap();
                assert!(record.reason.contains(why), "{why}: {}", record.reason);
                // A sent request may have spent its credit: cool down and skip.
                if attempts > 0 {
                    assert_eq!(record.outcome, "unconfirmed");
                    assert_eq!(log.cooldown_until(), Some(at + COOLDOWN));
                } else {
                    assert_eq!(record.outcome, "abandoned");
                    assert_eq!(log.cooldown_until(), None);
                }
            }
        }
    }

    #[test]
    fn a_retry_rechecks_fresh_usage_credit_expiry_and_wording() {
        let retry = |fresh: Cached| {
            let mut log = ResetLog {
                pending: Some(pending(1, AT)),
                ..Default::default()
            };
            let mut fake = Fake {
                before: Some(fresh),
                responses: vec![Ok(serde_json::json!({"outcome": "reset"}))],
                ..Default::default()
            };
            let step = resume_with(&mut fake, &mut log, exhausted_at(AT), AT);
            (step, log, fake)
        };
        // The fresh read shows room: the earlier attempt may have worked.
        let (held, log, fake) = retry(cache(0.0, Some(7), vec![]));
        assert!(matches!(
            held,
            Step::Held {
                hold: Hold::Changed { .. }
            }
        ));
        assert!(fake.redeemed.is_empty());
        assert_eq!(log.history.last().unwrap().outcome, "unconfirmed");
        // The credit expired since.
        let (held, log, fake) = retry(cache(100.0, Some(6), vec![credit("c6", Some(AT))]));
        assert!(matches!(
            held,
            Step::Held {
                hold: Hold::Changed { .. }
            }
        ));
        assert!(fake.redeemed.is_empty());
        assert!(
            log.history
                .last()
                .unwrap()
                .reason
                .contains("its credit has expired")
        );
        // Its wording now mentions money: refuse and halt.
        let mut priced = credit("c6", None);
        priced.description = Some("Purchase this reset".into());
        let (refused, log, fake) = retry(cache(100.0, Some(6), vec![priced]));
        assert_eq!(
            refused,
            Step::Refused {
                label: "six".into(),
                halted: true
            }
        );
        assert!(fake.redeemed.is_empty() && log.pending.is_none() && log.halted.is_some());
        // The earlier attempt may have spent: recorded unconfirmed, with a
        // cooldown and a skip, before the refusal.
        let outcomes: Vec<_> = log.history.iter().map(|r| r.outcome.as_str()).collect();
        assert_eq!(outcomes, ["unconfirmed", "refused"]);
        assert_eq!(log.cooldown_until(), Some(AT + COOLDOWN));
        assert!(log.skipped("six", AT + DAY));
        // The fresh read fails: wait, with the key kept.
        let mut log = ResetLog {
            pending: Some(pending(1, AT)),
            ..Default::default()
        };
        let mut failing = Fake::default();
        assert!(matches!(
            resume_with(&mut failing, &mut log, exhausted_at(AT), AT),
            Step::Waiting { .. }
        ));
        assert!(failing.redeemed.is_empty() && log.pending.is_some());
        // Unchanged, the same key and credit are sent again.
        let (sent, _, fake) = retry(cache(100.0, Some(6), vec![credit("c6", None)]));
        assert!(matches!(sent, Step::Reset { .. }));
        assert_eq!(fake.redeemed[0].1, "synthetic-key-1");
        assert_eq!(fake.redeemed[0].2, "c6");
    }

    #[test]
    fn only_codex_reset_off_clears_the_codex_halt_and_records_a_dropped_key() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path().join("store")).unwrap();
        let halted = ResetLog {
            pending: Some(pending(1, AT)),
            halted: Some("synthetic".into()),
            ..Default::default()
        };
        store::json_write(&log_path(&store), &halted).unwrap();
        let mut claude = Auto::default();
        // Claude: off is accepted and leaves the Codex log alone; others refuse.
        set_mode(&store, Provider::Claude, &mut claude, AutoReset::Off).unwrap();
        for mode in [AutoReset::DryRun, AutoReset::On] {
            assert!(set_mode(&store, Provider::Claude, &mut claude, mode).is_err());
        }
        assert_eq!(claude.auto_reset, AutoReset::Off);
        assert_eq!(load(&store).unwrap(), halted);
        // Codex dry-run and on keep the halt and the key.
        let mut codex = Auto::default();
        for mode in [AutoReset::DryRun, AutoReset::On] {
            set_mode(&store, Provider::Codex, &mut codex, mode).unwrap();
            assert_eq!(load(&store).unwrap(), halted);
        }
        // Codex off clears the halt and records the dropped key.
        set_mode(&store, Provider::Codex, &mut codex, AutoReset::Off).unwrap();
        let log = load(&store).unwrap();
        assert!(log.halted.is_none() && log.pending.is_none());
        let record = log.history.last().unwrap();
        assert_eq!(record.outcome, "unconfirmed");
        assert!(record.reason.contains("turned off"));
        assert!(log.cooldown_until().is_some());
        assert!(log.skipped("six", AT + DAY));
        // Off again changes nothing more.
        let before = load(&store).unwrap();
        set_mode(&store, Provider::Codex, &mut codex, AutoReset::Off).unwrap();
        assert_eq!(load(&store).unwrap(), before);
    }

    #[test]
    fn a_second_tick_skips_while_another_holds_the_reset_lock() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path().join("store")).unwrap();
        let mut state = store.load().unwrap();
        state
            .auto
            .insert(Provider::Codex, settings(AutoReset::DryRun));
        store.save(&state).unwrap();
        let held = exclusive(&store).unwrap().expect("first holder");
        assert!(exclusive(&store).unwrap().is_none());
        assert_eq!(tick(&store, Provider::Codex).unwrap(), None);
        assert!(!log_path(&store).exists());
        drop(held);
        assert!(exclusive(&store).unwrap().is_some());
    }

    #[test]
    fn the_log_is_capped_and_old_state_loads() {
        let mut log = ResetLog::default();
        for i in 0..(HISTORY as u64 + 7) {
            log.push(Record {
                at: i,
                label: "a".into(),
                outcome: "dry-run".into(),
                weekly_resets_at: None,
                reason: String::new(),
                pre: None,
                post: None,
            });
        }
        assert_eq!(log.history.len(), HISTORY);
        assert_eq!(log.history[0].at, 7);
        let empty: ResetLog = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, ResetLog::default());
        let old: Auto =
            serde_json::from_str(r#"{"enabled":true,"threshold":90,"last_switch":0}"#).unwrap();
        assert_eq!(old.auto_reset, AutoReset::Off);
    }
}
