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

#[test]
fn safety_reserved_pending_label_cannot_consume_or_hide_headroom() {
    for reconcile in [false, true] {
        let p = Pool::new();
        let a = p.add("pending", "synthetic-member-a");
        let b = p.add("unreserved-alias", "synthetic-member-a");
        let c = p.add("other", "synthetic-member-b");
        let at = aswap::now();
        let mut f = Fake::new(&p.store);
        f.response = json!({"synthetic_error":true});
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
        let retry = log.pending.as_ref().unwrap().retry_at;
        log.pending.as_mut().unwrap().reconcile = reconcile;
        let mut policy = settings(AutoReset::On);
        policy.reserved.push(a.label.clone());
        f.response = json!({"outcome":"reset"});
        f.readings.insert(a.label.clone(), cache(100.0, retry));
        let result = reset::step(
            &mut f,
            Provider::Codex,
            &policy,
            vec![
                (a, cache(100.0, retry)),
                (b, cache(20.0, retry)),
                (c, cache(100.0, retry)),
            ],
            &mut log,
            false,
            retry,
        )
        .unwrap();
        assert_eq!(
            f.consumed.len(),
            1,
            "reserved pending target consumed again: {result:?}"
        );
    }
}
#[test]
fn safety_other_pending_request_respects_unreserved_alias_headroom() {
    for reconcile in [false, true] {
        let p = Pool::new();
        let a = p.add("reserved", "synthetic-member-a");
        let b = p.add("usable", "synthetic-member-a");
        let c = p.add("pending-other", "synthetic-member-b");
        let at = aswap::now();
        let mut f = Fake::new(&p.store);
        f.response = json!({"synthetic_error":true});
        f.readings.insert(c.label.clone(), cache(100.0, at));
        let mut log = ResetLog::default();
        reset::step(
            &mut f,
            Provider::Codex,
            &settings(AutoReset::On),
            vec![(c.clone(), cache(100.0, at))],
            &mut log,
            false,
            at,
        )
        .unwrap();
        let retry = log.pending.as_ref().unwrap().retry_at;
        log.pending.as_mut().unwrap().reconcile = reconcile;
        let mut policy = settings(AutoReset::On);
        policy.reserved.push(a.label.clone());
        f.response = json!({"outcome":"reset"});
        f.readings.insert(c.label.clone(), cache(100.0, retry));
        reset::step(
            &mut f,
            Provider::Codex,
            &policy,
            vec![
                (a, cache(20.0, retry)),
                (b, cache(20.0, retry)),
                (c, cache(100.0, retry)),
            ],
            &mut log,
            false,
            retry,
        )
        .unwrap();
        assert_eq!(
            f.consumed.len(),
            1,
            "unreserved alias headroom must stop pending attempt"
        );
    }
}
#[test]
fn safety_known_skip_must_not_follow_label_to_a_different_member() {
    let p = Pool::new();
    let old = p.add("reused", "synthetic-member-a");
    let at = aswap::now();
    let mut f = Fake::new(&p.store);
    f.readings.insert(old.label.clone(), cache(100.0, at));
    let mut log = ResetLog::default();
    reset::step(
        &mut f,
        Provider::Codex,
        &settings(AutoReset::On),
        vec![(old.clone(), cache(100.0, at))],
        &mut log,
        false,
        at,
    )
    .unwrap();
    assert_eq!(f.consumed.len(), 1);
    let old_key = f.account_key(&old);
    assert!(log.skipped.iter().any(|s| s.account == old_key));
    p.remove(&old);
    let bob = p.add("reused", "synthetic-member-b");
    let new_key = f.account_key(&bob);
    assert_ne!(old_key, new_key);
    f.readings.insert(bob.label.clone(), cache(100.0, at + 1));
    let result = reset::step(
        &mut f,
        Provider::Codex,
        &settings(AutoReset::On),
        vec![(bob, cache(100.0, at + 1))],
        &mut log,
        false,
        at + 1,
    )
    .unwrap();
    assert_eq!(
        f.consumed.len(),
        2,
        "Alice's known-key skip incorrectly blocked a different workspace member using her old label: {result:?}"
    );
}
fn intent(label: &str, account: Option<String>, key: &str, at: u64) -> reset::Pending {
    serde_json::from_value(json!({"label":label,"credit_id":"synthetic-credit","idempotency_key":key,"weekly_resets_at":at+86400,"reason":"synthetic","started_at":at,"attempts":0,"retry_at":at,"account":account})).unwrap()
}
#[test]
fn safety_replay_must_retain_both_accounts_when_one_label_was_reused() {
    let p = Pool::new();
    let alice = p.add("alice-alias", "synthetic-member-a");
    let bob = p.add("reused", "synthetic-member-b");
    let at = aswap::now();
    let mut f = Fake::new(&p.store);
    let alice_key = f.account_key(&alice).unwrap();
    let bob_key = f.account_key(&bob).unwrap();
    for (account, key, time) in [
        (alice_key.clone(), "synthetic-alice-key", at),
        (bob_key.clone(), "synthetic-bob-key", at + 1),
    ] {
        reset::append(
            &p.store,
            &reset::Event::Intent {
                at: time,
                pending: intent("reused", Some(account), key, time),
            },
        )
        .unwrap();
        reset::append(
            &p.store,
            &reset::Event::Attempt {
                at: time,
                key: key.into(),
                n: 1,
            },
        )
        .unwrap();
        reset::append(
            &p.store,
            &reset::Event::Outcome {
                at: time,
                key: key.into(),
                outcome: "no-credit".into(),
            },
        )
        .unwrap();
    }
    let replayed = {
        let _l = p.store.lock().unwrap();
        reset::load(&p.store).unwrap()
    };
    assert!(
        replayed
            .skipped
            .iter()
            .any(|s| s.account.as_ref() == Some(&alice_key)),
        "Bob's later use of same label deleted Alice's account-keyed skip: {:?}",
        replayed.skipped
    );
    assert!(
        replayed
            .skipped
            .iter()
            .any(|s| s.account.as_ref() == Some(&bob_key))
    );
}
#[test]
fn safety_legacy_journal_without_account_fields_replays_current_aliases() {
    let p = Pool::new();
    let a = p.add("old", "synthetic-member-a");
    let b = p.add("alias", "synthetic-member-a");
    let at = aswap::now();
    let mut pending = serde_json::to_value(intent("old", None, "synthetic-old-key", at)).unwrap();
    pending.as_object_mut().unwrap().remove("account");
    for event in [
        json!({"event":"intent","at":at,"pending":pending}),
        json!({"event":"outcome","at":at,"key":"synthetic-old-key","outcome":"nothing-to-reset"}),
    ] {
        let parsed: reset::Event = serde_json::from_value(event).unwrap();
        reset::append(&p.store, &parsed).unwrap();
    }
    let mut replayed = {
        let _l = p.store.lock().unwrap();
        reset::load(&p.store).unwrap()
    };
    assert!(replayed.skipped.iter().all(|s| s.account.is_none()));
    let mut f = Fake::new(&p.store);
    f.readings.insert(a.label.clone(), cache(100.0, at + 1));
    f.readings.insert(b.label.clone(), cache(100.0, at + 1));
    reset::step(
        &mut f,
        Provider::Codex,
        &settings(AutoReset::On),
        vec![(a, cache(100.0, at + 1)), (b, cache(100.0, at + 1))],
        &mut replayed,
        false,
        at + 1,
    )
    .unwrap();
    assert!(f.consumed.is_empty());
}

#[test]
fn safety_legacy_pending_without_account_fields_keeps_its_key() {
    let p = Pool::new();
    let a = p.add("old", "synthetic-member-a");
    let b = p.add("alias", "synthetic-member-a");
    let at = aswap::now();
    let mut f = Fake::new(&p.store);
    f.response = json!({"synthetic_error":true});
    f.readings.insert(a.label.clone(), cache(100.0, at));
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
    let pending = log.pending.as_ref().unwrap().clone();
    let mut snapshot = serde_json::to_value(&log).unwrap();
    snapshot["pending"]
        .as_object_mut()
        .unwrap()
        .remove("account");
    store::json_write(&reset::log_path(&p.store), &snapshot).unwrap();
    let text = std::fs::read_to_string(reset::journal_path(&p.store)).unwrap();
    let legacy = text
        .lines()
        .map(|line| {
            let mut v: Value = serde_json::from_str(line).unwrap();
            if let Some(p) = v.get_mut("pending").and_then(Value::as_object_mut) {
                p.remove("account");
            }
            serde_json::to_string(&v).unwrap()
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(reset::journal_path(&p.store), legacy).unwrap();
    let mut restored = {
        let _l = p.store.lock().unwrap();
        reset::load(&p.store).unwrap()
    };
    assert!(restored.pending.as_ref().unwrap().account.is_none());
    assert_eq!(
        restored.pending.as_ref().unwrap().idempotency_key,
        pending.idempotency_key
    );
    f.response = json!({"outcome":"noCredit"});
    f.readings
        .insert(a.label.clone(), cache(100.0, pending.retry_at));
    reset::step(
        &mut f,
        Provider::Codex,
        &settings(AutoReset::On),
        vec![(a, cache(100.0, pending.retry_at)), (b, Cached::default())],
        &mut restored,
        false,
        pending.retry_at,
    )
    .unwrap();
    assert_eq!(f.consumed.len(), 2);
    assert_eq!(f.keys, 1);
    let text = std::fs::read_to_string(reset::journal_path(&p.store)).unwrap();
    for line in text.lines() {
        let event: reset::Event = serde_json::from_str(line).unwrap();
        if let reset::Event::Attempt { key, .. } = event {
            assert_eq!(key, pending.idempotency_key);
        }
    }
}

#[test]
fn legacy_memberless_alias_must_share_the_known_accounts_skip() {
    let p = Pool::new();
    let known = p.add("known", "synthetic-member-a");
    let legacy = p.add("legacy", "synthetic-member-a");
    let at = aswap::now();
    for (a, member) in [(&known, Some("synthetic-member-a")), (&legacy, None)] {
        let mut claims = json!({"chatgpt_account_id":"synthetic-workspace","chatgpt_user_id":"synthetic-user-a"});
        if let Some(m) = member {
            claims["chatgpt_account_user_id"] = m.into();
        }
        let payload = json!({"https://api.openai.com/auth":claims});
        let jwt = format!(
            "synthetic.{}.synthetic",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap())
        );
        store::json_write(
            &p.store
                .profile(a.provider, &a.label)
                .unwrap()
                .join("auth.json"),
            &json!({"tokens":{"account_id":"synthetic-workspace","access_token":jwt}}),
        )
        .unwrap();
    }
    let mut f = Fake::new(&p.store);
    assert_eq!(f.aliases(&known), vec!["legacy"]);
    assert!(f.account_key(&known).is_some());
    assert!(f.account_key(&legacy).is_none());
    f.response = json!({"outcome":"noCredit"});
    f.readings.insert(known.label.clone(), cache(100.0, at));
    f.readings
        .insert(legacy.label.clone(), cache(100.0, at + 1));
    let mut log = ResetLog::default();
    reset::step(
        &mut f,
        Provider::Codex,
        &settings(AutoReset::On),
        vec![
            (known.clone(), cache(100.0, at)),
            (legacy.clone(), cache(100.0, at)),
        ],
        &mut log,
        false,
        at,
    )
    .unwrap();
    assert_eq!(f.consumed, vec!["known"]);
    // Same group and quota, only the legacy label is now first in the view.
    reset::step(
        &mut f,
        Provider::Codex,
        &settings(AutoReset::On),
        vec![
            (legacy, cache(100.0, at + 1)),
            (known, cache(100.0, at + 1)),
        ],
        &mut log,
        false,
        at + 1,
    )
    .unwrap();
    assert_eq!(
        f.consumed.len(),
        1,
        "a fallback-matched same-account alias ignored its group member's known-key skip: {:?}",
        f.consumed
    );
}

#[test]
fn legacy_representative_must_persist_a_known_siblings_account_key() {
    let p = Pool::new();
    let known = p.add("known", "synthetic-member-a");
    let legacy = p.add("legacy", "synthetic-member-a");
    let at = aswap::now();
    for (a, member) in [(&known, Some("synthetic-member-a")), (&legacy, None)] {
        let mut claims = json!({"chatgpt_account_id":"synthetic-workspace","chatgpt_user_id":"synthetic-user-a"});
        if let Some(m) = member {
            claims["chatgpt_account_user_id"] = m.into();
        }
        let payload = json!({"https://api.openai.com/auth":claims});
        let jwt = format!(
            "synthetic.{}.synthetic",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap())
        );
        store::json_write(
            &p.store
                .profile(a.provider, &a.label)
                .unwrap()
                .join("auth.json"),
            &json!({"tokens":{"account_id":"synthetic-workspace","access_token":jwt}}),
        )
        .unwrap();
    }
    let mut f = Fake::new(&p.store);
    assert_eq!(f.aliases(&legacy), vec!["known"]);
    f.response = json!({"outcome":"noCredit"});
    f.readings.insert(known.label.clone(), cache(100.0, at + 1));
    f.readings.insert(legacy.label.clone(), cache(100.0, at));
    let mut log = ResetLog::default();
    reset::step(
        &mut f,
        Provider::Codex,
        &settings(AutoReset::On),
        vec![
            (legacy.clone(), cache(100.0, at)),
            (known.clone(), cache(100.0, at)),
        ],
        &mut log,
        false,
        at,
    )
    .unwrap();
    assert_eq!(f.consumed, vec!["legacy"]);
    std::fs::remove_file(reset::log_path(&p.store)).unwrap();
    p.remove(&legacy);
    let mut replayed = {
        let _l = p.store.lock().unwrap();
        reset::load(&p.store).unwrap()
    };
    reset::step(
        &mut f,
        Provider::Codex,
        &settings(AutoReset::On),
        vec![(known, cache(100.0, at + 1))],
        &mut replayed,
        false,
        at + 1,
    )
    .unwrap();
    assert_eq!(
        f.consumed.len(),
        1,
        "the selected legacy alias lost a key available on its group sibling; replay consumed again: {:?}",
        f.consumed
    );
}

#[test]
fn review_legacy_bridge_must_not_merge_two_known_members() {
    let p = Pool::new();
    let legacy = p.add("legacy", "unused");
    let alice = p.add("alice", "synthetic-member-a");
    let bob = p.add("bob", "synthetic-member-b");
    let at = aswap::now();
    for (a, member) in [
        (&legacy, None),
        (&alice, Some("synthetic-member-a")),
        (&bob, Some("synthetic-member-b")),
    ] {
        let mut claims = json!({"chatgpt_account_id":"synthetic-workspace","chatgpt_user_id":"synthetic-user-a"});
        if let Some(m) = member {
            claims["chatgpt_account_user_id"] = m.into();
        }
        let payload = json!({"https://api.openai.com/auth":claims});
        let jwt = format!(
            "synthetic.{}.synthetic",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap())
        );
        store::json_write(
            &p.store
                .profile(a.provider, &a.label)
                .unwrap()
                .join("auth.json"),
            &json!({"tokens":{"account_id":"synthetic-workspace","access_token":jwt}}),
        )
        .unwrap();
    }
    let mut f = Fake::new(&p.store);
    assert!(f.aliases(&alice).iter().all(|x| x != "bob"));
    assert_eq!(f.aliases(&legacy), vec!["alice", "bob"]);
    f.response = json!({"outcome":"noCredit"});
    for a in [&legacy, &alice, &bob] {
        f.readings.insert(a.label.clone(), cache(100.0, at));
    }
    let mut log = ResetLog::default();
    reset::step(
        &mut f,
        Provider::Codex,
        &settings(AutoReset::On),
        vec![
            (legacy, cache(100.0, at)),
            (alice, cache(100.0, at)),
            (bob, cache(100.0, at)),
        ],
        &mut log,
        false,
        at,
    )
    .unwrap();
    assert!(
        f.consumed.is_empty(),
        "ambiguous legacy group carries two distinct authoritative keys but consume was attempted: {:?}",
        f.consumed
    );
}
