//! Pure recovery decisions for supervised runs: which account a limited
//! session may move to, when to look again, and how long rejections last.
use super::now;
use crate::{
    Result,
    provider::Provider,
    store::{self, Account, Store},
    usage,
};
use std::time::{Duration, Instant};

pub(super) fn quota_fallback(
    account: &Account,
    entries: &[(Account, usage::Cached)],
    settings: &store::Auto,
    at: u64,
    pinned: Option<&str>,
) -> Option<(f64, f64)> {
    if !settings.allows(&account.label, pinned) {
        return None;
    }
    let used = entries
        .iter()
        .find(|(a, _)| a.provider == account.provider && a.label == account.label)?
        .1
        .used(at)?;
    (used >= settings.threshold && used < 100.0).then_some((used, settings.threshold))
}

pub(super) fn report_quota_fallback(account: &Account, fallback: Option<(f64, f64)>) {
    if let Some((used, threshold)) = fallback {
        eprintln!(
            "aswap: last-resort quota fallback to {}:{} ({used:.0}% used, soft threshold {threshold:.0}%); using remaining quota instead of waiting for reset",
            account.provider, account.label
        );
    }
}

/// A limited provider can resume on its own account once that account's
/// rejection lapses. Other targets retain the usual forced-switch ranking
/// and below-100% fallback. `Cached::used` requires fresh coverage of every
/// known exhausted window, including in the caches reloaded at commit.
pub(super) fn recovery_target<'a>(
    current: &str,
    entries: &'a [(Account, usage::Cached)],
    settings: &store::Auto,
    at: u64,
    pinned: Option<&str>,
) -> Option<&'a Account> {
    let usable = |label| {
        entries.iter().find_map(|(account, cache)| {
            (account.label == label
                && settings.allows(&account.label, pinned)
                && cache.used(at).is_some_and(|used| used < 100.0))
            .then_some(account)
        })
    };
    pinned
        .and_then(usable)
        .or_else(|| usage::choose(current, entries, settings, at, true, pinned))
        .or_else(|| usable(current))
}

/// Pure wall-clock schedule, also used with synthetic time in tests. Shared
/// quota caching serializes provider requests; per-supervisor jitter spreads
/// the cache reads and resumed CLI launches after a common reset.
pub(super) struct Wake {
    next: u64,
    stagger: u64,
    reported: Option<u64>,
}
impl Wake {
    pub(super) fn new(seed: u64) -> Self {
        Self {
            next: 0,
            stagger: 1 + seed % 30,
            reported: None,
        }
    }
    pub(super) fn due(&self, at: u64) -> bool {
        at >= self.next
    }
    pub(super) fn schedule(
        &mut self,
        entries: &[(Account, usage::Cached)],
        hold: Option<u64>,
        at: u64,
    ) {
        let next = entries
            .iter()
            .flat_map(|(_, cache)| {
                cache
                    .reset_times()
                    .chain(std::iter::once(cache.next_poll_at))
            })
            .chain(hold)
            .filter(|when| *when > at)
            .min()
            .unwrap_or(at.saturating_add(300))
            .min(at.saturating_add(300));
        self.next = next.saturating_add(self.stagger);
    }
    pub(super) fn retry(&mut self, at: u64) {
        self.next = at.saturating_add(6 + self.stagger % 5);
    }
    pub(super) fn report(&mut self, provider: Provider, current: &str, at: u64) {
        if self.reported != Some(self.next) {
            eprintln!(
                "aswap: {provider}:{current} is waiting on a usage limit; watching all accounts, next quota/reset check in {}s",
                self.next.saturating_sub(at)
            );
            self.reported = Some(self.next);
        }
    }
}

// Run-local rejection survives owner cache replacement and refresh observations.
// Keep identity keys private/in-memory so aliases cannot bounce a session back
// onto credentials that this provider process has already rejected.
// A login or model rejection lasts for the run. A limit rejection lapses (see
// `Lapse`), so one limit, or one misread, can't strand a long session once
// every other account is exhausted too.
#[derive(Default)]
pub(super) struct Rejected {
    labels: std::collections::BTreeMap<String, Option<Lapse>>,
    identities: std::collections::BTreeMap<String, Option<Lapse>>,
}
/// When a limit rejection lapses: after a hold that doubles with each new
/// limit on the account (15 minutes, then 30, ... up to 4 hours), and only once
/// a reading taken after the rejection is below the auto threshold. A verified
/// window reset permits remaining quota above that soft threshold too. Without
/// a reset, near-full accounts cannot trade a session back and forth on the
/// strength of repeated, misleading quota readings.
#[derive(Clone, Copy)]
pub(super) struct Lapse {
    at: u64,
    count: u32,
    /// A known window boundary when the account rejected the session. Only a
    /// fresh, successful post-reset observation may release the hold early.
    reset: Option<u64>,
    resumed: bool,
}
pub(super) const LIMIT_HOLD: u64 = 15 * 60;
pub(super) const LIMIT_HOLD_MAX: u64 = 4 * 3600;
impl Lapse {
    pub(super) fn hold(&self) -> u64 {
        (LIMIT_HOLD << self.count.saturating_sub(1).min(8)).min(LIMIT_HOLD_MAX)
    }
    pub(super) fn lapsed(&self, cache: &usage::Cached, threshold: f64, at: u64) -> bool {
        let reset = self
            .reset
            .is_some_and(|reset| at >= reset && cache.fetched_at >= reset);
        (at >= self.at.saturating_add(self.hold()) || reset)
            && cache.fetched_at > self.at
            && cache
                .used(at)
                .is_some_and(|used| used < if reset { 100.0 } else { threshold })
    }
    /// The same limit seen again while held stays one rejection; a limit
    /// after the rejection lapsed is a new one, held twice as long.
    pub(super) fn again(previous: Option<Self>, at: u64) -> Self {
        match previous {
            Some(p) if !p.resumed && at < p.at.saturating_add(p.hold()) => p,
            Some(p) => Self {
                at,
                count: p.count.saturating_add(1),
                reset: None,
                resumed: false,
            },
            None => Self {
                at,
                count: 1,
                reset: None,
                resumed: false,
            },
        }
    }
}
impl Rejected {
    /// Consuming a verified reset is one recovery. If this new provider hits
    /// a limit too, begin a new, doubled hold even before the old hold ends.
    pub(super) fn resumed(&mut self, store: &Store, account: &Account, accounts: &[Account]) {
        for label in std::iter::once(account.label.clone())
            .chain(crate::identity::duplicates(store, account, accounts))
        {
            if let Some(Some(lapse)) = self.labels.get_mut(&label) {
                lapse.resumed = true;
            }
        }
        if let Some(key) = Self::key(store, account)
            && let Some(Some(lapse)) = self.identities.get_mut(&key)
        {
            lapse.resumed = true;
        }
    }
    /// Reject `account` (and its aliases and identity) for the run, or, for
    /// a `limit`, until the rejection lapses.
    pub(super) fn reject(&mut self, store: &Store, account: &Account, limit: bool) -> Result<()> {
        let at = now();
        let mut keys = vec![account.label.clone()];
        let (identity, reset) = {
            let _lock = store.lock()?;
            keys.extend(crate::identity::duplicates(
                store,
                account,
                &store.load()?.accounts,
            ));
            let path = store
                .profile(account.provider, &account.label)?
                .join("usage-cache.json");
            let cache: usage::Cached = if path.exists() {
                store::json_read(&path)?
            } else {
                usage::Cached::default()
            };
            (
                Self::key(store, account),
                cache.reset_times().filter(|reset| *reset > at).min(),
            )
        };
        let mark = |map: &mut std::collections::BTreeMap<String, Option<Lapse>>, key: String| {
            let next = match map.get(&key) {
                // A login or model rejection is never weakened by a limit.
                Some(None) => None,
                _ if !limit => None,
                previous => {
                    let mut lapse = Lapse::again(previous.copied().flatten(), at);
                    if lapse.at == at {
                        lapse.reset = reset;
                    }
                    Some(lapse)
                }
            };
            map.insert(key, next);
        };
        for key in keys {
            mark(&mut self.labels, key);
        }
        if let Some(key) = identity {
            mark(&mut self.identities, key);
        }
        Ok(())
    }
    pub(super) fn key(store: &Store, account: &Account) -> Option<String> {
        crate::identity::read(
            account.provider,
            &store.profile(account.provider, &account.label).ok()?,
        )
        .ok()?
        .lock_key()
    }
    pub(super) fn filter(
        &self,
        store: &Store,
        current: &str,
        threshold: f64,
        entries: Vec<(Account, usage::Cached)>,
    ) -> Vec<(Account, usage::Cached)> {
        self.filter_at(store, current, threshold, entries, now())
    }
    pub(super) fn next_lapse(&self, at: u64) -> Option<u64> {
        self.labels
            .values()
            .chain(self.identities.values())
            .flatten()
            .map(|lapse| lapse.at.saturating_add(lapse.hold()))
            .filter(|when| *when > at)
            .min()
    }
    pub(super) fn filter_at(
        &self,
        store: &Store,
        current: &str,
        threshold: f64,
        entries: Vec<(Account, usage::Cached)>,
        at: u64,
    ) -> Vec<(Account, usage::Cached)> {
        let held = |rejection: Option<&Option<Lapse>>, cache: &usage::Cached| match rejection {
            None => false,
            Some(None) => true,
            Some(Some(lapse)) => !lapse.lapsed(cache, threshold, at),
        };
        entries
            .into_iter()
            .filter_map(|(a, mut cache)| {
                let rejected = held(self.labels.get(&a.label), &cache)
                    || Self::key(store, &a)
                        .is_some_and(|key| held(self.identities.get(&key), &cache));
                if rejected && super::trace::trace_selection_enabled() {
                    eprintln!(
                        "aswap: candidate={}:{} reason=run-rejected",
                        a.provider, a.label
                    );
                }
                if a.label == current {
                    // choose() needs the current entry even while rejected,
                    // but same-account recovery must respect its hold too.
                    if rejected {
                        cache.error = Some("run-rejected".into());
                    }
                    Some((a, cache))
                } else {
                    (!rejected).then_some((a, cache))
                }
            })
            .collect()
    }
}
/// Automatic restarts are bounded per hour rather than per run, so a session
/// that lives for days can still switch when its accounts reset, while a run
/// misled by its observations stops after eight quick restarts.
#[derive(Default)]
pub(super) struct Restarts(std::collections::VecDeque<Instant>);
pub(super) const RESTARTS: usize = 8;
pub(super) const RESTART_WINDOW: Duration = Duration::from_secs(3600);
impl Restarts {
    pub(super) fn capped(&mut self) -> bool {
        while self
            .0
            .front()
            .is_some_and(|at| at.elapsed() >= RESTART_WINDOW)
        {
            self.0.pop_front();
        }
        self.0.len() >= RESTARTS
    }
    pub(super) fn record(&mut self) {
        self.0.push_back(Instant::now());
    }
}

#[cfg(test)]
mod rejection_tests {
    use super::*;

    fn cache(used: f64, fetched_at: u64) -> usage::Cached {
        usage::Cached {
            usage: usage::Usage {
                windows: vec![usage::Window {
                    name: "seven_day".into(),
                    used_percent: used,
                    resets_at: None,
                    duration_minutes: Some(10080),
                    codex_slot: None,
                }],
                ..Default::default()
            },
            fetched_at,
            ..Default::default()
        }
    }

    #[test]
    fn a_limit_rejection_lapses_after_its_hold_on_a_fresh_reading_below_threshold() {
        let lapse = Lapse {
            at: 10_000,
            count: 1,
            reset: None,
            resumed: false,
        };
        let later = 10_000 + LIMIT_HOLD;
        // Held for the hold, however good the reading.
        assert!(!lapse.lapsed(&cache(5.0, later - 10), 90.0, later - 1));
        assert!(lapse.lapsed(&cache(5.0, later - 10), 90.0, later));
        // Only a reading taken after the rejection, below the threshold, and
        // without an error counts.
        assert!(!lapse.lapsed(&cache(5.0, 9_999), 90.0, 10_500 + LIMIT_HOLD));
        assert!(!lapse.lapsed(&cache(95.0, later), 90.0, later));
        let mut failed = cache(5.0, later);
        failed.error = Some("provider-limit".into());
        assert!(!lapse.lapsed(&failed, 90.0, later));
    }

    #[test]
    fn a_new_limit_after_a_lapse_is_held_twice_as_long() {
        let first = Lapse::again(None, 1_000);
        assert_eq!(
            (first.at, first.count, first.hold()),
            (1_000, 1, LIMIT_HOLD)
        );
        // Seen again every tick while held: still the same rejection.
        let same = Lapse::again(Some(first), 1_000 + LIMIT_HOLD - 1);
        assert_eq!((same.at, same.count), (1_000, 1));
        let second = Lapse::again(Some(first), 1_000 + LIMIT_HOLD);
        assert_eq!((second.count, second.hold()), (2, 2 * LIMIT_HOLD));
        let many = Lapse {
            at: 0,
            count: 30,
            reset: None,
            resumed: false,
        };
        assert_eq!(many.hold(), LIMIT_HOLD_MAX);
    }

    #[test]
    fn a_new_limit_after_an_early_reset_starts_a_new_doubled_hold() {
        let first = Lapse {
            at: 1_000,
            count: 1,
            reset: Some(1_030),
            resumed: true,
        };
        let next = Lapse::again(Some(first), 1_031);
        assert_eq!(
            (next.at, next.count, next.hold()),
            (1_031, 2, 2 * LIMIT_HOLD)
        );
        assert_eq!(next.reset, None);
        assert!(!next.lapsed(&cache(2.0, 1_032), 90.0, 1_032));
    }

    #[test]
    fn accounts_near_the_limit_never_trade_a_session_back_and_forth() {
        let t = tempfile::tempdir().unwrap();
        let store = Store::open(t.path().join("store")).unwrap();
        let account = |label: &str| Account {
            provider: Provider::Claude,
            label: label.into(),
        };
        let at = now();
        let mut rejected = Rejected::default();
        // "a" hit its limit two hours ago (its hold is over); now on "b".
        rejected.labels.insert(
            "a".into(),
            Some(Lapse {
                at: at - 7200,
                count: 1,
                reset: None,
                resumed: false,
            }),
        );
        rejected.labels.insert("expired".into(), None);
        let kept = |entries: Vec<(Account, usage::Cached)>| {
            rejected
                .filter(&store, "b", 90.0, entries)
                .into_iter()
                .map(|(a, _)| a.label)
                .collect::<Vec<_>>()
        };
        // Still near 100%: "a" stays rejected, so "b" can't bounce back to it.
        assert_eq!(
            kept(vec![
                (account("a"), cache(98.0, at)),
                (account("b"), cache(99.0, at))
            ]),
            vec!["b"]
        );
        // "a" reset (or the limit was misread): it is a candidate again.
        assert_eq!(
            kept(vec![
                (account("a"), cache(3.0, at)),
                (account("b"), cache(99.0, at))
            ]),
            vec!["a", "b"]
        );
        // A login rejection never lapses.
        assert_eq!(
            kept(vec![
                (account("expired"), cache(3.0, at)),
                (account("b"), cache(99.0, at))
            ]),
            vec!["b"]
        );
    }

    #[test]
    fn restarts_are_capped_per_hour_not_per_run() {
        let mut restarts = Restarts::default();
        for _ in 0..RESTARTS {
            assert!(!restarts.capped());
            restarts.record();
        }
        assert!(restarts.capped());
        // An hour later the same session may switch again (unless this
        // machine has been up for less than an hour).
        if let Some(old) = Instant::now().checked_sub(RESTART_WINDOW) {
            restarts.0.iter_mut().for_each(|at| *at = old);
            assert!(!restarts.capped());
        }
    }
}

#[cfg(test)]
mod wake_tests {
    use super::*;
    use crate::runner::monitor::entries_from_cache;

    const AT: u64 = 10_000;

    fn entry(
        provider: Provider,
        label: &str,
        used: f64,
        reset: Option<u64>,
        at: u64,
    ) -> (Account, usage::Cached) {
        (
            Account {
                provider,
                label: label.into(),
            },
            usage::Cached {
                usage: usage::Usage {
                    windows: vec![usage::Window {
                        name: "weekly".into(),
                        used_percent: used,
                        resets_at: reset,
                        duration_minutes: Some(10080),
                        codex_slot: None,
                    }],
                    ..Default::default()
                },
                fetched_at: at,
                next_poll_at: at + usage::TTL,
                ..Default::default()
            },
        )
    }

    #[test]
    fn every_selection_path_prefers_the_soonest_weekly_reset_for_both_providers() {
        let settings = store::Auto {
            prefer: store::Prefer::WeeklyReset,
            ..Default::default()
        };
        for provider in [Provider::Claude, Provider::Codex] {
            let entries = [
                entry(provider, "current", 100.0, Some(AT + 4 * 86_400), AT),
                entry(provider, "later", 20.0, Some(AT + 5 * 86_400), AT),
                entry(provider, "sooner", 60.0, Some(AT + 86_400), AT),
                entry(provider, "unknown", 5.0, None, AT),
            ];
            let label = |a: Option<&Account>| a.map(|a| a.label.clone());
            // Launch and the monitor tick: a forced switch off an exhausted account.
            assert_eq!(
                label(usage::choose(
                    "current", &entries, &settings, AT, false, None
                )),
                Some("sooner".into()),
                "{provider}"
            );
            // Quota fallback after a provider limit, and supervisor recovery.
            assert_eq!(
                label(usage::choose(
                    "current", &entries, &settings, AT, true, None
                )),
                Some("sooner".into()),
                "{provider}"
            );
            assert_eq!(
                label(recovery_target("current", &entries, &settings, AT, None)),
                Some("sooner".into()),
                "{provider}"
            );
            // Launch preference and the monitor move off a later-resetting account.
            for current in ["later", "unknown"] {
                assert_eq!(
                    label(usage::prefer_at_launch(current, &entries, &settings, AT)),
                    Some("sooner".into()),
                    "{provider} {current}"
                );
            }
            assert!(usage::prefer_at_launch("sooner", &entries, &settings, AT).is_none());
            // Headroom ranks by most room instead.
            let headroom = store::Auto {
                prefer: store::Prefer::Headroom,
                ..Default::default()
            };
            assert_eq!(
                label(recovery_target("current", &entries, &headroom, AT, None)),
                Some("unknown".into())
            );
        }
    }

    #[test]
    fn recovery_only_uses_a_reserved_label_for_its_pin() {
        let mut entries = [
            entry(Provider::Claude, "held", 95.0, Some(AT + 1000), AT),
            entry(Provider::Claude, "other", 100.0, Some(AT + 1800), AT),
        ];
        let settings = store::Auto {
            reserved: vec!["held".into()],
            ..Default::default()
        };
        assert!(recovery_target("held", &entries, &settings, AT, None).is_none());
        assert!(quota_fallback(&entries[0].0, &entries, &settings, AT, None).is_none());
        for current in ["held", "other"] {
            assert_eq!(
                recovery_target(current, &entries, &settings, AT, Some("held"))
                    .map(|a| a.label.as_str()),
                Some("held")
            );
        }
        entries[0].1.usage.windows[0].used_percent = 100.0;
        entries[1].1.usage.windows[0].used_percent = 95.0;
        assert_eq!(
            recovery_target("held", &entries, &settings, AT, Some("held"))
                .map(|a| a.label.as_str()),
            Some("other")
        );
    }

    #[test]
    fn recovery_rechecks_weekly_quota_from_the_locked_cache_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path().join("store")).unwrap();
        let settings = store::Auto {
            threshold: 90.0,
            prefer: store::Prefer::WeeklyReset,
            ..Default::default()
        };
        let (current, exhausted) = entry(Provider::Claude, "current", 100.0, None, AT);
        let (first, verified) = entry(Provider::Claude, "chatgpt1", 61.0, Some(AT + 6000), AT);
        let (third, _) = entry(Provider::Claude, "chatgpt3", 100.0, None, AT);
        let mut state = store.load().unwrap();
        state.accounts = vec![current.clone(), first.clone(), third.clone()];
        store.save(&state).unwrap();
        let write_cache = |account: &Account, cache: &usage::Cached| {
            let path = store.profile(account.provider, &account.label).unwrap();
            store::json_write(&path.join("usage-cache.json"), cache).unwrap();
        };
        write_cache(&current, &exhausted);
        write_cache(&first, &verified);
        let full = |weekly: f64| {
            usage::parse_claude(&serde_json::json!({
                "five_hour": {"utilization":39},
                "seven_day": {
                    "utilization":weekly,
                    "resets_at":chrono::DateTime::from_timestamp((AT + 3000) as i64, 0)
                        .unwrap().to_rfc3339()
                }
            }))
            .unwrap()
        };
        let partial = usage::parse_claude(&serde_json::json!({
            "five_hour": {"utilization":39}, "seven_day":null
        }))
        .unwrap();
        let select = |rejected: &Rejected| {
            let _lock = store.lock().unwrap();
            let entries = entries_from_cache(&store, Provider::Claude).unwrap();
            let entries = rejected.filter_at(&store, "current", settings.threshold, entries, AT);
            recovery_target("current", &entries, &settings, AT, None).map(|a| a.label.clone())
        };
        let rejected = Rejected::default();
        let usable = usage::updated(usage::Cached::default(), Ok(full(80.0)), AT - 120);
        write_cache(&third, &usable);
        assert_eq!(select(&rejected).as_deref(), Some("chatgpt3"));
        let stale = usage::updated(usage::Cached::default(), Ok(full(80.0)), AT - 601);
        write_cache(&third, &stale);
        assert_eq!(select(&rejected).as_deref(), Some("chatgpt1"));

        let full = usage::updated(usable, Ok(full(100.0)), AT - 60);
        write_cache(&third, &full);
        assert_eq!(select(&rejected).as_deref(), Some("chatgpt1"));
        let mut held = Rejected::default();
        let lapse = Lapse {
            at: AT - 60,
            count: 1,
            reset: Some(AT),
            resumed: false,
        };
        held.labels.insert(third.label.clone(), Some(lapse));
        assert_eq!(select(&held).as_deref(), Some("chatgpt1"));
        let verified_session_reset = usage::updated(full, Ok(partial.clone()), AT);
        write_cache(&third, &verified_session_reset);
        assert!(!lapse.lapsed(&verified_session_reset, 90.0, AT));
        assert_eq!(select(&held).as_deref(), Some("chatgpt1"));
        assert_eq!(select(&rejected).as_deref(), Some("chatgpt1"));

        let no_weekly_history = usage::updated(usage::Cached::default(), Ok(partial), AT);
        write_cache(&third, &no_weekly_history);
        assert_eq!(select(&rejected).as_deref(), Some("chatgpt1"));

        write_cache(&third, &verified_session_reset);
        write_cache(&first, &usage::Cached::default());
        assert_eq!(select(&rejected), None);
    }

    #[test]
    fn wake_watches_every_accounts_weekly_and_session_reset() {
        for provider in [Provider::Claude, Provider::Codex] {
            let mut entries = vec![
                entry(provider, "current", 100.0, Some(AT + 200), AT),
                entry(provider, "other", 100.0, Some(AT + 60), AT),
            ];
            entries[0].1.usage.windows.push(usage::Window {
                name: "session".into(),
                used_percent: 100.0,
                resets_at: Some(AT + 30),
                duration_minutes: Some(300),
                codex_slot: None,
            });
            let mut wake = Wake::new(17);
            wake.schedule(&entries, None, AT);
            assert!((AT + 31..=AT + 60).contains(&wake.next));
            assert!(!wake.due(AT + 29));
            assert!(wake.due(AT + 60));
        }
    }

    #[test]
    fn unknown_and_past_resets_keep_polling_without_spinning() {
        let mut wake = Wake::new(42);
        for reset in [None, Some(AT - 1)] {
            let mut entries = vec![entry(Provider::Claude, "current", 100.0, reset, AT - 600)];
            entries[0].1.next_poll_at = AT - 1;
            wake.schedule(&entries, None, AT);
            assert!((AT + 301..=AT + 330).contains(&wake.next));
            assert!(!wake.due(AT + 1));
        }
        wake.schedule(&[], Some(AT + 20), AT);
        assert!((AT + 21..=AT + 50).contains(&wake.next));
    }

    #[test]
    fn stalled_sessions_stagger_checks_and_observe_normal_cache_refreshes() {
        let entries = vec![entry(Provider::Codex, "current", 100.0, None, AT)];
        let times = (0..32)
            .map(|seed| {
                let mut wake = Wake::new(seed);
                wake.schedule(&entries, None, AT);
                assert!((AT + usage::TTL + 1..=AT + usage::TTL + 30).contains(&wake.next));
                wake.next
            })
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(times.len(), 30);
    }

    #[test]
    fn verified_reset_recovers_remaining_quota_at_or_above_the_soft_threshold() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path().join("store")).unwrap();
        for provider in [Provider::Claude, Provider::Codex] {
            for recovered in ["current", "other"] {
                for used in [90.0, 95.0] {
                    let mut rejected = Rejected::default();
                    rejected.labels.insert(
                        recovered.into(),
                        Some(Lapse {
                            at: AT,
                            count: 3,
                            reset: Some(AT + 30),
                            resumed: false,
                        }),
                    );
                    let mut entries = vec![
                        entry(provider, "current", 100.0, Some(AT + 3600), AT + 30),
                        entry(provider, "other", 100.0, Some(AT + 3600), AT + 30),
                    ];
                    // The five-hour window reset, but the weekly window
                    // still reads at or above the soft switching threshold.
                    let (_, cache) = entries
                        .iter_mut()
                        .find(|(a, _)| a.label == recovered)
                        .unwrap();
                    cache.usage.windows[0].used_percent = used;
                    cache.usage.windows.push(usage::Window {
                        name: "session".into(),
                        used_percent: 0.0,
                        resets_at: Some(AT + 30 + 5 * 3600),
                        duration_minutes: Some(300),
                        codex_slot: None,
                    });
                    let entries = rejected.filter_at(&store, "current", 90.0, entries, AT + 30);
                    assert_eq!(
                        recovery_target(
                            "current",
                            &entries,
                            &store::Auto::default(),
                            AT + 30,
                            None
                        )
                        .map(|a| a.label.as_str()),
                        Some(recovered),
                        "{provider} {recovered} at {used}%"
                    );
                }
            }
        }
    }

    #[test]
    fn only_verified_post_reset_quota_releases_a_hold_early() {
        let lapse = Lapse {
            at: AT,
            count: 3,
            reset: Some(AT + 30),
            resumed: false,
        };
        let (_, mut fresh) = entry(Provider::Claude, "a", 4.0, Some(AT + 3600), AT + 30);
        assert!(lapse.lapsed(&fresh, 90.0, AT + 30));
        fresh.fetched_at = AT + 29;
        assert!(!lapse.lapsed(&fresh, 90.0, AT + 30));
        fresh.fetched_at = AT + 30;
        fresh.usage.windows[0].used_percent = 95.0;
        assert!(lapse.lapsed(&fresh, 90.0, AT + 30));
        fresh.usage.windows[0].used_percent = 100.0;
        assert!(!lapse.lapsed(&fresh, 90.0, AT + 30));
        fresh.usage.windows[0].used_percent = 4.0;
        fresh.error = Some("network".into());
        assert!(!lapse.lapsed(&fresh, 90.0, AT + 30));
    }

    #[test]
    fn the_first_verified_account_wakes_the_session_even_when_it_is_current() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path().join("store")).unwrap();
        for provider in [Provider::Claude, Provider::Codex] {
            for first in ["current", "other"] {
                let mut rejected = Rejected::default();
                for label in ["current", "other"] {
                    rejected.labels.insert(
                        label.into(),
                        Some(Lapse {
                            at: AT,
                            count: 1,
                            reset: Some(AT + if label == first { 30 } else { 60 }),
                            resumed: false,
                        }),
                    );
                }
                let mut entries = vec![
                    entry(provider, "current", 100.0, Some(AT + 30), AT),
                    entry(provider, "other", 100.0, Some(AT + 30), AT),
                ];
                let choose = |entries, at| {
                    recovery_target(
                        "current",
                        &rejected.filter_at(&store, "current", 90.0, entries, at),
                        &store::Auto::default(),
                        at,
                        None,
                    )
                    .map(|a| a.label.clone())
                };
                // A timestamp passing is not evidence of usable quota.
                assert_eq!(choose(entries.clone(), AT + 30), None);
                let recovered = entries.iter_mut().find(|(a, _)| a.label == first).unwrap();
                recovered.1 = entry(provider, first, 3.0, Some(AT + 3600), AT + 30).1;
                assert_eq!(choose(entries, AT + 30).as_deref(), Some(first));
            }
        }
    }

    #[test]
    fn same_account_recovery_keeps_ordinary_holds_and_permanent_rejections() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path().join("store")).unwrap();
        let mut rejected = Rejected::default();
        rejected.labels.insert(
            "current".into(),
            Some(Lapse {
                at: AT,
                count: 1,
                reset: None,
                resumed: false,
            }),
        );
        let choose = |rejected: &Rejected, at| {
            let entries = rejected.filter_at(
                &store,
                "current",
                90.0,
                vec![entry(Provider::Codex, "current", 5.0, None, at)],
                at,
            );
            recovery_target("current", &entries, &store::Auto::default(), at, None)
                .map(|a| a.label.clone())
        };
        assert_eq!(choose(&rejected, AT + 10), None);
        assert_eq!(
            choose(&rejected, AT + LIMIT_HOLD).as_deref(),
            Some("current")
        );
        rejected.labels.insert("current".into(), None);
        assert_eq!(choose(&rejected, AT + LIMIT_HOLD_MAX), None);
    }
}
