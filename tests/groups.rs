//! t479's review probes for #50: list/status grouping.
#![allow(dead_code, unused_imports)]
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

fn run(pool: &Pool, args: &[&str]) -> String {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_aswap"))
        .args(args)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("HOME", pool._temp.path())
        .env("XDG_DATA_HOME", pool._temp.path().join("data"))
        .env("XDG_CONFIG_HOME", pool._temp.path().join("config"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}
fn seed(pool: &Pool) {
    for a in pool.accounts() {
        store::json_write(
            &pool
                .store
                .profile(a.provider, &a.label)
                .unwrap()
                .join("usage-cache.json"),
            &Cached {
                usage: reading(20.0),
                fetched_at: aswap::now(),
                next_poll_at: aswap::now() + 3600,
                ..Default::default()
            },
        )
        .unwrap();
    }
}
fn bridge() -> Pool {
    let p = Pool::new();
    let legacy = p.add(Provider::Codex, "legacy", "unused");
    let alice = p.add(Provider::Codex, "alice", "synthetic-member-a");
    let bob = p.add(Provider::Codex, "bob", "synthetic-member-b");
    for (a, member) in [
        (&legacy, None),
        (&alice, Some("synthetic-member-a")),
        (&bob, Some("synthetic-member-b")),
    ] {
        let mut c = json!({"chatgpt_account_id":"synthetic-workspace","chatgpt_user_id":"synthetic-user-a"});
        if let Some(m) = member {
            c["chatgpt_account_user_id"] = m.into();
        }
        let jwt = format!(
            "synthetic.{}.synthetic",
            URL_SAFE_NO_PAD
                .encode(serde_json::to_vec(&json!({"https://api.openai.com/auth":c})).unwrap())
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
    seed(&p);
    p
}
#[test]
fn review_ambiguous_list_must_warn_instead_of_claiming_both_members() {
    let p = bridge();
    let text = run(&p, &["list"]);
    assert!(
        text.to_lowercase().contains("ambiguous"),
        "ambiguous identity not disclosed: {text}"
    );
    assert!(
        !text.contains("same login as alice, bob"),
        "asserted both members same login: {text}"
    );
}
#[test]
fn review_ambiguous_status_must_warn_instead_of_claiming_both_members() {
    let p = bridge();
    let text = run(&p, &["status"]);
    assert!(
        text.to_lowercase().contains("ambiguous"),
        "ambiguous identity not disclosed: {text}"
    );
}
#[test]
fn review_json_is_additive_and_waybar_keeps_output_shape() {
    let p = Pool::new();
    p.add(Provider::Codex, "work", "synthetic-member-a");
    p.add(Provider::Pi, "same", "synthetic-member-a");
    p.add(Provider::Pi, "teammate", "synthetic-member-b");
    seed(&p);
    let v: serde_json::Value = serde_json::from_str(&run(&p, &["list", "--json"])).unwrap();
    assert_eq!(v["schema_version"], 1);
    let a = v["accounts"].as_array().unwrap();
    assert_eq!(a[0]["same_account"], json!(["pi:same"]));
    assert_eq!(a[2]["same_account"], json!([]));
    let text = run(&p, &["status"]);
    assert!(text.contains("same account as pi:same"), "{text}");
    let w: serde_json::Value = serde_json::from_str(&run(&p, &["waybar", "--once"])).unwrap();
    assert_eq!(w["class"], "aswap");
    for k in ["text", "tooltip"] {
        assert!(w[k].is_string());
    }
}

#[test]
fn ambiguous_json_names_the_candidates_and_drops_same_account() {
    let p = bridge();
    let v: serde_json::Value = serde_json::from_str(&run(&p, &["list", "--json"])).unwrap();
    let row = |label: &str| {
        v["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["label"] == label)
            .unwrap()
            .clone()
    };
    assert_eq!(row("legacy")["ambiguous_with"], json!(["alice", "bob"]));
    assert!(row("legacy")["same_account"].is_null());
    assert_eq!(row("alice")["ambiguous_with"], json!([]));
    let text = run(&p, &["list"]);
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        flat.contains(
            "ambiguous: matches alice and bob (different members); re-sign or remove legacy"
        ),
        "{text}"
    );
    assert!(
        flat.contains("ambiguous login legacy may be this account"),
        "{text}"
    );
}

#[test]
fn review_ambiguous_labels_must_be_scoped_to_their_program() {
    let p = bridge();
    p.add(Provider::Pi, "legacy", "synthetic-member-pi");
    p.add(Provider::Pi, "pi-alias", "synthetic-member-pi");
    seed(&p);
    let v: serde_json::Value = serde_json::from_str(&run(&p, &["list", "--json"])).unwrap();
    let pi = v["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["provider"] == "pi" && r["label"] == "pi-alias")
        .unwrap();
    assert_eq!(pi["ambiguous_with"], json!([]));
    assert_eq!(pi["duplicates"], json!(["legacy"]));
    for command in ["list", "status"] {
        let text = run(&p, &[command]);
        assert!(
            text.contains("same login as legacy"),
            "a Codex label made the unrelated, unambiguous pi duplicate look ambiguous: {text}"
        );
    }
}
