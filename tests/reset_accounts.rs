//! t479's review probes for #49: one account in the reset ledger.
#![allow(clippy::single_element_loop)]
use aswap::{
    identity,
    provider::Provider,
    reset::{self, Port, ResetLog},
    store::{self, Account, Auto, AutoReset, Store},
    usage::{Cached, ResetCredit, Usage, Window},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use std::collections::BTreeMap;
struct Pool {
    _tmp: tempfile::TempDir,
    store: Store,
}
impl Pool {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path().join("store")).unwrap();
        Self { _tmp: tmp, store }
    }
    fn add(&self, label: &str, member: &str) -> Account {
        let a = Account {
            provider: Provider::Codex,
            label: label.into(),
        };
        let p = self.store.profile(a.provider, label).unwrap();
        store::private_dir(&p).unwrap();
        let c = json!({"https://api.openai.com/auth":{"chatgpt_account_id":"synthetic-workspace","chatgpt_account_user_id":member}});
        let jwt = format!(
            "synthetic.{}.synthetic",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&c).unwrap())
        );
        store::json_write(
            &p.join("auth.json"),
            &json!({"tokens":{"account_id":"synthetic-workspace","access_token":jwt}}),
        )
        .unwrap();
        let mut s = self.store.load().unwrap();
        s.accounts.push(a.clone());
        self.store.save(&s).unwrap();
        a
    }
    fn remove(&self, a: &Account) {
        let _l = self.store.lock().unwrap();
        let mut s = self.store.load().unwrap();
        s.accounts.retain(|x| x.label != a.label);
        self.store.save(&s).unwrap();
        std::fs::remove_dir_all(self.store.profile(a.provider, &a.label).unwrap()).unwrap();
    }
}
fn cache(used: f64, at: u64) -> Cached {
    Cached {
        usage: Usage {
            windows: vec![Window {
                name: "weekly".into(),
                used_percent: used,
                resets_at: Some(at + 86400),
                duration_minutes: Some(10080),
                codex_slot: Some(1),
            }],
            available_usage_resets: Some(1),
            reset_credits_listed: true,
            reset_credits: vec![ResetCredit {
                id: "synthetic-credit".into(),
                status: "available".into(),
                reset_type: "codexRateLimits".into(),
                expires_at: Some(at + 86400),
                title: Some("Full reset (Weekly + 5 hr)".into()),
                description: Some("Ready to redeem".into()),
            }],
            ..Default::default()
        },
        fetched_at: at,
        next_poll_at: at + 180,
        ..Default::default()
    }
}
struct Fake<'a> {
    store: &'a Store,
    readings: BTreeMap<String, Cached>,
    consumed: Vec<String>,
    response: Value,
    keys: u32,
}
impl<'a> Fake<'a> {
    fn new(store: &'a Store) -> Self {
        Self {
            store,
            readings: BTreeMap::new(),
            consumed: Vec::new(),
            response: json!({"outcome":"nothingToReset"}),
            keys: 0,
        }
    }
}
impl Port for Fake<'_> {
    fn refresh(&mut self, a: &Account) -> aswap::Result<Cached> {
        self.readings
            .get(&a.label)
            .cloned()
            .ok_or(aswap::Error("synthetic unread login"))
    }
    fn redeem(&mut self, a: &Account, _: &str, _: &str) -> aswap::Result<Value> {
        self.consumed.push(a.label.clone());
        if self.response["synthetic_error"] == true {
            Err(aswap::Error("synthetic lost response"))
        } else {
            Ok(self.response.clone())
        }
    }
    fn save(&mut self, l: &ResetLog) -> aswap::Result<()> {
        store::json_write(&reset::log_path(self.store), l)
    }
    fn new_key(&mut self) -> aswap::Result<String> {
        self.keys += 1;
        Ok(format!("synthetic-key-{}", self.keys))
    }
    fn journal(&mut self, e: &reset::Event) -> aswap::Result<()> {
        reset::append(self.store, e)
    }
    fn mode(&mut self) -> aswap::Result<AutoReset> {
        Ok(AutoReset::On)
    }
    fn aliases(&mut self, a: &Account) -> Vec<String> {
        let _l = self.store.lock().unwrap();
        identity::duplicates(self.store, a, &self.store.load().unwrap().accounts)
    }
    fn pause(&mut self, _: u64) {}
    fn account_key(&mut self, a: &Account) -> Option<String> {
        identity::read(a.provider, &self.store.profile(a.provider, &a.label).ok()?)
            .ok()?
            .account_key()
    }
}
fn settings(mode: AutoReset) -> Auto {
    Auto {
        auto_reset: mode,
        ..Default::default()
    }
}
#[test]
fn review_reserved_representative_must_not_hide_unreserved_headroom() {
    let p = Pool::new();
    let reserved = p.add("reserved", "synthetic-member-a");
    let usable = p.add("usable", "synthetic-member-a");
    let full = p.add("other", "synthetic-member-b");
    let at = aswap::now();
    let mut f = Fake::new(&p.store);
    f.response = json!({"outcome":"reset"});
    f.readings.insert(full.label.clone(), cache(100.0, at));
    let mut policy = settings(AutoReset::On);
    policy.reserved.push(reserved.label.clone());
    let result = reset::step(
        &mut f,
        Provider::Codex,
        &policy,
        vec![
            (reserved, cache(20.0, at)),
            (usable, cache(20.0, at)),
            (full, cache(100.0, at)),
        ],
        &mut ResetLog::default(),
        false,
        at,
    )
    .unwrap();
    assert!(
        f.consumed.is_empty(),
        "unreserved alias has20% quota but reset step called consume {:?}: {result:?}",
        f.consumed
    );
}
#[test]
fn review_replay_after_source_removed_must_keep_surviving_alias_skipped() {
    for outcome in ["nothingToReset"] {
        let p = Pool::new();
        let a = p.add("first", "synthetic-member-a");
        let b = p.add("alias", "synthetic-member-a");
        let at = aswap::now();
        let mut f = Fake::new(&p.store);
        f.response = json!({"outcome":outcome});
        f.readings.insert(a.label.clone(), cache(100.0, at));
        f.readings.insert(b.label.clone(), cache(100.0, at));
        let mut log = ResetLog::default();
        reset::step(
            &mut f,
            Provider::Codex,
            &settings(AutoReset::On),
            vec![(a.clone(), cache(100.0, at)), (b.clone(), cache(100.0, at))],
            &mut log,
            false,
            at,
        )
        .unwrap();
        assert_eq!(f.consumed, vec!["first"]);
        // Simulate losing the snapshot after the durable outcome, then removing
        // the original profile while its same-account alias remains.
        std::fs::remove_file(reset::log_path(&p.store)).unwrap();
        p.remove(&a);
        let mut replayed = {
            let _l = p.store.lock().unwrap();
            reset::load(&p.store).unwrap()
        };
        reset::step(
            &mut f,
            Provider::Codex,
            &settings(AutoReset::On),
            vec![(b, cache(100.0, at + 1))],
            &mut replayed,
            false,
            at + 1,
        )
        .unwrap();
        assert_eq!(
            f.consumed.len(),
            1,
            "{outcome}: replay lost account identity and called consume again via the surviving alias: {:?}",
            f.consumed
        );
    }
}
#[test]
fn review_no_credit_replay_after_source_removed_must_keep_alias_skipped() {
    for outcome in ["noCredit"] {
        let p = Pool::new();
        let a = p.add("first", "synthetic-member-a");
        let b = p.add("alias", "synthetic-member-a");
        let at = aswap::now();
        let mut f = Fake::new(&p.store);
        f.response = json!({"outcome":outcome});
        f.readings.insert(a.label.clone(), cache(100.0, at));
        f.readings.insert(b.label.clone(), cache(100.0, at));
        let mut log = ResetLog::default();
        reset::step(
            &mut f,
            Provider::Codex,
            &settings(AutoReset::On),
            vec![(a.clone(), cache(100.0, at)), (b.clone(), cache(100.0, at))],
            &mut log,
            false,
            at,
        )
        .unwrap();
        assert_eq!(f.consumed, vec!["first"]);
        // Simulate losing the snapshot after the durable outcome, then removing
        // the original profile while its same-account alias remains.
        std::fs::remove_file(reset::log_path(&p.store)).unwrap();
        p.remove(&a);
        let mut replayed = {
            let _l = p.store.lock().unwrap();
            reset::load(&p.store).unwrap()
        };
        reset::step(
            &mut f,
            Provider::Codex,
            &settings(AutoReset::On),
            vec![(b, cache(100.0, at + 1))],
            &mut replayed,
            false,
            at + 1,
        )
        .unwrap();
        assert_eq!(
            f.consumed.len(),
            1,
            "{outcome}: replay lost account identity and called consume again via the surviving alias: {:?}",
            f.consumed
        );
    }
}
#[test]
fn review_decline_skip_must_follow_public_rename() {
    let p = Pool::new();
    let a = p.add("before", "synthetic-member-a");
    let at = aswap::now();
    let mut f = Fake::new(&p.store);
    f.readings.insert(a.label.clone(), cache(100.0, at));
    let mut log = ResetLog::default();
    reset::step(
        &mut f,
        Provider::Codex,
        &settings(AutoReset::On),
        vec![(a.clone(), cache(100.0, at))],
        &mut log,
        false,
        at,
    )
    .unwrap();
    assert_eq!(f.consumed.len(), 1);
    let b = aswap::rename::rename(&p.store, Provider::Codex, "before", "after").unwrap();
    f.readings.insert(b.label.clone(), cache(100.0, at + 1));
    let mut reloaded = {
        let _l = p.store.lock().unwrap();
        reset::load(&p.store).unwrap()
    };
    reset::step(
        &mut f,
        Provider::Codex,
        &settings(AutoReset::On),
        vec![(b, cache(100.0, at + 1))],
        &mut reloaded,
        false,
        at + 1,
    )
    .unwrap();
    assert_eq!(
        f.consumed.len(),
        1,
        "the same account renamed was consumed for again: {:?}",
        f.consumed
    );
}

#[test]
fn review_pending_retry_guard_must_also_count_aliases_once() {
    let p = Pool::new();
    let a = p.add("readable", "synthetic-member-a");
    let b = p.add("unread", "synthetic-member-a");
    let at = aswap::now();
    let mut f = Fake::new(&p.store);
    f.readings.insert(a.label.clone(), cache(100.0, at));
    f.response = json!({"synthetic_error":true});
    let mut log = ResetLog::default();
    reset::step(
        &mut f,
        Provider::Codex,
        &settings(AutoReset::On),
        vec![
            (a.clone(), cache(100.0, at)),
            (b.clone(), Cached::default()),
        ],
        &mut log,
        false,
        at,
    )
    .unwrap();
    assert_eq!(f.consumed.len(), 1);
    let retry = log.pending.as_ref().unwrap().retry_at;
    f.response = json!({"outcome":"nothingToReset"});
    f.readings.insert(a.label.clone(), cache(100.0, retry));
    let outcome = reset::step(
        &mut f,
        Provider::Codex,
        &settings(AutoReset::On),
        vec![(a, cache(100.0, retry)), (b, Cached::default())],
        &mut log,
        false,
        retry,
    )
    .unwrap();
    assert_eq!(
        f.consumed.len(),
        2,
        "pending retry still counts unread alias separately and never sends saved key: {outcome:?}"
    );
}

#[test]
fn review_static_aliases_rank_once_and_teammates_stay_separate() {
    let p = Pool::new();
    let a = p.add("a", "synthetic-member-a");
    let b = p.add("a-alias", "synthetic-member-a");
    let teammate = p.add("teammate", "synthetic-member-b");
    let at = aswap::now();
    let mut f = Fake::new(&p.store);
    let mut later = cache(100.0, at);
    later.usage.windows[0].resets_at = Some(at + 2 * 86400);
    let result = reset::step(
        &mut f,
        Provider::Codex,
        &settings(AutoReset::DryRun),
        vec![
            (a, cache(100.0, at)),
            (b, cache(100.0, at)),
            (teammate, later),
        ],
        &mut ResetLog::default(),
        false,
        at,
    )
    .unwrap();
    assert!(
        matches!(&result,reset::Step::WouldReset{decision,..} if decision.label=="teammate" && decision.reason.contains("all 2 usable")),
        "{result:?}"
    );
    assert!(f.consumed.is_empty());
}
#[test]
fn review_live_declines_skip_current_aliases_for_both_outcomes() {
    for outcome in ["nothingToReset", "noCredit"] {
        let p = Pool::new();
        let a = p.add("first", "synthetic-member-a");
        let b = p.add("alias", "synthetic-member-a");
        let at = aswap::now();
        let mut f = Fake::new(&p.store);
        f.response = json!({"outcome":outcome});
        f.readings.insert(a.label.clone(), cache(100.0, at));
        f.readings.insert(b.label.clone(), cache(100.0, at));
        let mut log = ResetLog::default();
        reset::step(
            &mut f,
            Provider::Codex,
            &settings(AutoReset::On),
            vec![(a.clone(), cache(100.0, at)), (b.clone(), cache(100.0, at))],
            &mut log,
            false,
            at,
        )
        .unwrap();
        reset::step(
            &mut f,
            Provider::Codex,
            &settings(AutoReset::On),
            vec![
                (b.clone(), cache(100.0, at + 1)),
                (a.clone(), cache(100.0, at + 1)),
            ],
            &mut log,
            false,
            at + 1,
        )
        .unwrap();
        assert_eq!(
            f.consumed.len(),
            1,
            "{outcome}: unchanged alias must not waste a second consume call"
        );
        assert!(log.skipped.iter().any(|s| s.label == b.label));
    }
}
