//! One real account across programs (asw-xs4 step 3): Codex and pi's
//! ChatGPT login of one workspace membership share quota and limits, never
//! credentials. Synthetic tokens only; no network.
use aswap::{
    identity::{bindings, duplicates},
    provider::Provider,
    store::{self, Account, Store},
    usage::{self, Cached, Usage, Window},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::json;

fn jwt(member: &str) -> String {
    let claims = json!({"https://api.openai.com/auth":{
        "chatgpt_account_id":"synthetic-workspace","chatgpt_account_user_id":member}});
    format!(
        "synthetic.{}.synthetic",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
    )
}

struct Pool {
    _temp: tempfile::TempDir,
    store: Store,
}
impl Pool {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path().join("data/aswap")).unwrap();
        Self { _temp: temp, store }
    }
    fn add(&self, provider: Provider, label: &str, member: &str) -> Account {
        let account = Account {
            provider,
            label: label.into(),
        };
        let profile = self.store.profile(provider, label).unwrap();
        store::private_dir(&profile).unwrap();
        let auth = match provider {
            Provider::Pi => json!({"openai-codex":{"type":"oauth","access":jwt(member),
                "refresh":format!("synthetic-refresh-{label}"),"expires":4_000_000_000_000u64,
                "accountId":"synthetic-workspace"}}),
            _ => json!({"tokens":{"account_id":"synthetic-workspace","access_token":jwt(member),
                "refresh_token":format!("synthetic-refresh-{label}")}}),
        };
        store::json_write(&profile.join(provider.auth_file()), &auth).unwrap();
        let mut state = self.store.load().unwrap();
        state.accounts.push(account.clone());
        self.store.save(&state).unwrap();
        account
    }
    fn cache(&self, account: &Account) -> Cached {
        let path = self
            .store
            .profile(account.provider, &account.label)
            .unwrap()
            .join("usage-cache.json");
        if path.exists() {
            store::json_read(&path).unwrap()
        } else {
            Cached::default()
        }
    }
    fn accounts(&self) -> Vec<Account> {
        self.store.load().unwrap().accounts
    }
}
fn labels(accounts: Vec<Account>) -> Vec<String> {
    accounts
        .iter()
        .map(|a| format!("{}/{}", a.provider, a.label))
        .collect()
}
fn reading(used: f64) -> Usage {
    Usage {
        windows: vec![Window {
            name: "weekly".into(),
            used_percent: used,
            resets_at: Some(aswap::now() + 86_400),
            duration_minutes: Some(10080),
            codex_slot: Some(1),
        }],
        ..Default::default()
    }
}

#[test]
fn codex_and_pi_logins_of_one_member_are_bound_but_never_share_credentials() {
    let pool = Pool::new();
    let codex = pool.add(Provider::Codex, "work", "synthetic-member-a");
    let pi = pool.add(Provider::Pi, "work-pi", "synthetic-member-a");
    let teammate = pool.add(Provider::Pi, "bob", "synthetic-member-b");
    let _lock = pool.store.lock().unwrap();
    let accounts = pool.accounts();
    assert_eq!(
        labels(bindings(&pool.store, &codex, &accounts)),
        ["pi/work-pi"]
    );
    assert_eq!(
        labels(bindings(&pool.store, &pi, &accounts)),
        ["codex/work"]
    );
    assert!(bindings(&pool.store, &teammate, &accounts).is_empty());
    // Credentials stay per program: no alias across programs, so a refresh
    // is never propagated from one program's login into the other's.
    assert!(duplicates(&pool.store, &codex, &accounts).is_empty());
    assert!(duplicates(&pool.store, &pi, &accounts).is_empty());
}

#[test]
fn one_programs_reading_is_the_other_programs_reading() {
    let pool = Pool::new();
    let codex = pool.add(Provider::Codex, "work", "synthetic-member-a");
    let pi = pool.add(Provider::Pi, "work-pi", "synthetic-member-a");
    let teammate = pool.add(Provider::Pi, "bob", "synthetic-member-b");
    let at = aswap::now();
    // Codex reads the account: pi's binding gets it, the teammate doesn't.
    usage::cached_mode_with(&pool.store, &codex, at, true, |_| Ok(reading(42.0))).unwrap();
    assert_eq!(pool.cache(&pi).usage.windows[0].used_percent, 42.0);
    assert_eq!(pool.cache(&pi).fetched_at, at);
    assert!(pool.cache(&teammate).usage.windows.is_empty());
    // pi's next check uses it instead of fetching again.
    let seen = usage::cached_mode_with(&pool.store, &pi, at + 1, false, |_| {
        panic!("one account is read once")
    })
    .unwrap();
    assert_eq!(seen.usage.windows[0].used_percent, 42.0);
}

#[test]
fn a_program_with_no_fresh_reading_reuses_the_other_programs() {
    let pool = Pool::new();
    let codex = pool.add(Provider::Codex, "work", "synthetic-member-a");
    let pi = pool.add(Provider::Pi, "work-pi", "synthetic-member-a");
    let at = aswap::now();
    // A reading Codex left (written as an older aswap would, unshared).
    let fresh = Cached {
        usage: reading(17.0),
        fetched_at: at,
        next_poll_at: at + 180,
        last_attempt: at,
        ..Default::default()
    };
    store::json_write(
        &pool
            .store
            .profile(codex.provider, &codex.label)
            .unwrap()
            .join("usage-cache.json"),
        &fresh,
    )
    .unwrap();
    let seen = usage::cached_mode_with(&pool.store, &pi, at + 1, false, |_| {
        panic!("one account is read once")
    })
    .unwrap();
    assert_eq!(seen.usage.windows[0].used_percent, 17.0);
}

#[test]
fn a_limit_in_one_program_holds_the_account_in_the_other() {
    let pool = Pool::new();
    let codex = pool.add(Provider::Codex, "work", "synthetic-member-a");
    let pi = pool.add(Provider::Pi, "work-pi", "synthetic-member-a");
    let teammate = pool.add(Provider::Codex, "bob", "synthetic-member-b");
    usage::mark_limited(&pool.store, &pi).unwrap();
    let marked = |a: &Account| pool.cache(a).error.as_deref() == Some("provider-limit");
    assert!(marked(&pi) && marked(&codex));
    assert!(!marked(&teammate));
}
