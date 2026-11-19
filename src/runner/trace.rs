//! Selection tracing (`ASWAP_TRACE_SELECTION=1`): why each candidate account
//! was or wasn't picked. Only numeric window fields and fixed
//! classifications are printed, never stored names or error strings.
use super::rotation::rotated_since;
use crate::{
    store::{self, Account, Store},
    usage,
};

pub(super) fn trace_selection_enabled() -> bool {
    std::env::var("ASWAP_TRACE_SELECTION").as_deref() == Ok("1")
}

pub(super) fn trace_recovery(
    store: &Store,
    account: &Account,
    launched: Option<&[u8]>,
    entries: &[(Account, usage::Cached)],
    settings: &store::Auto,
    stage: &str,
    at: u64,
) {
    if trace_selection_enabled() {
        trace_candidates(
            stage,
            entries,
            settings.threshold,
            at,
            Some(rotated_since(store, account, launched)),
        );
    }
}

pub(super) fn trace_candidates(
    stage: &str,
    entries: &[(Account, usage::Cached)],
    threshold: f64,
    at: u64,
    changed: Option<bool>,
) {
    if !trace_selection_enabled() {
        return;
    }
    if let Some(changed) = changed {
        eprintln!("aswap: selection={stage} credentials_changed_since_launch={changed}");
    } else {
        eprintln!("aswap: selection={stage}");
    }
    for (account, cache) in entries {
        trace_candidate(account, cache, threshold, at);
    }
}

pub(super) fn trace_candidate(account: &Account, cache: &usage::Cached, threshold: f64, at: u64) {
    let age = (cache.fetched_at != 0).then(|| at.saturating_sub(cache.fetched_at));
    let reason = match cache.error.as_deref() {
        Some("model-unavailable") => "model-unavailable",
        Some("provider-limit") => "provider-limit",
        Some(_) => "quota-read-failed",
        None if !cache.unverified_limits.is_empty() => "unverified-hard-limit",
        None if age.is_none_or(|age| age > 600) => "missing-or-stale-quota",
        None => match cache.used(at) {
            None => "missing-or-reset-window",
            Some(used) if used >= 100.0 => "quota-exhausted",
            Some(used) if used >= threshold => "at-or-above-soft-threshold",
            Some(_) => "quota-available",
        },
    };
    // Window names and cached error strings can come from stored data. Only
    // emit numeric window fields and the classifications above.
    let windows: Vec<_> = cache
        .usage
        .windows
        .iter()
        .map(|window| {
            (
                window.duration_minutes,
                window.used_percent,
                window.resets_at,
            )
        })
        .collect();
    eprintln!(
        "aswap: candidate={}:{} reason={reason} fetched_at_age_seconds={age:?} windows_minutes_used_percent_resets_at={windows:?}",
        account.provider, account.label,
    );
}
