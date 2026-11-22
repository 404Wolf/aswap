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

// t479's review probes for #48 (failing before the fix).
#[test]
fn review_rotations_never_write_the_other_programs_auth() {
    let pool = Pool::new();
    let codex = pool.add(Provider::Codex, "native", "synthetic-member-a");
    let pi = pool.add(Provider::Pi, "pi", "synthetic-member-a");
    store::json_write(
        &pool.store.root.join("coordination.json"),
        &json!({"owner":true}),
    )
    .unwrap();
    for (source, target) in [(&codex, &pi), (&pi, &codex)] {
        let path = pool
            .store
            .profile(target.provider, &target.label)
            .unwrap()
            .join(target.provider.auth_file());
        let before = std::fs::read(&path).unwrap();
        let _lock = pool.store.lock().unwrap();
        assert!(aswap::refresh::refresh_request_with(&pool.store,source,aswap::now(),true,|p,request| {
            assert_eq!(p,source.provider);
            assert!(request.body["refresh_token"].as_str().unwrap().contains(&source.label));
            Ok(json!({"access_token":jwt("synthetic-member-a"),"refresh_token":format!("synthetic-rotated-{}",source.label),"expires_in":3600}))
        }).unwrap());
        assert_eq!(before, std::fs::read(&path).unwrap());
    }
}
#[test]
fn review_limits_hold_bindings_in_both_directions() {
    for source_provider in [Provider::Codex, Provider::Pi] {
        let pool = Pool::new();
        let codex = pool.add(Provider::Codex, "native", "synthetic-member-a");
        let pi = pool.add(Provider::Pi, "pi", "synthetic-member-a");
        let bob = pool.add(Provider::Pi, "bob", "synthetic-member-b");
        let source = if source_provider == Provider::Codex {
            &codex
        } else {
            &pi
        };
        usage::mark_limited(&pool.store, source).unwrap();
        assert!(pool.cache(&codex).used(aswap::now()).is_none());
        assert!(pool.cache(&pi).used(aswap::now()).is_none());
        assert_eq!(pool.cache(&codex).error.as_deref(), Some("provider-limit"));
        assert_eq!(pool.cache(&pi).error.as_deref(), Some("provider-limit"));
        assert!(pool.cache(&bob).error.is_none());
    }
}
#[test]
fn review_shared_quota_must_not_clear_another_bindings_auth_failure() {
    let pool = Pool::new();
    let codex = pool.add(Provider::Codex, "native", "synthetic-member-a");
    let pi = pool.add(Provider::Pi, "pi", "synthetic-member-a");
    usage::mark_auth_expired(&pool.store, &codex).unwrap();
    usage::cached_mode_with(&pool.store, &pi, aswap::now() + 1, true, |_| {
        Ok(reading(10.0))
    })
    .unwrap();
    assert_eq!(
        pool.cache(&codex).error.as_deref(),
        Some("http-401"),
        "pi's valid login cannot repair native Codex's rejected independent token"
    );
}
#[test]
fn review_shared_quota_must_not_erase_another_bindings_retry_after() {
    let pool = Pool::new();
    let codex = pool.add(Provider::Codex, "native", "synthetic-member-a");
    let pi = pool.add(Provider::Pi, "pi", "synthetic-member-a");
    let at = aswap::now();
    usage::cached_mode_with(&pool.store, &codex, at, true, |_| {
        Err(aswap::usage::Failure::http(429, 900))
    })
    .unwrap();
    usage::cached_mode_with(&pool.store, &pi, at + 1, true, |_| Ok(reading(10.0))).unwrap();
    assert_eq!(pool.cache(&codex).retry_after, Some(900));
    assert!(pool.cache(&codex).next_poll_at >= at + 900);
}
#[test]
fn review_same_second_reset_reading_must_reach_the_held_pi_binding() {
    let pool = Pool::new();
    let codex = pool.add(Provider::Codex, "native", "synthetic-member-a");
    let pi = pool.add(Provider::Pi, "pi", "synthetic-member-a");
    let at = aswap::now();
    usage::cached_mode_with(&pool.store, &pi, at, true, |_| Ok(reading(100.0))).unwrap();
    usage::mark_limited(&pool.store, &pi).unwrap();
    usage::cached_mode_with(&pool.store, &codex, at, true, |_| Ok(reading(95.0))).unwrap();
    let seen = usage::cached_mode_with(&pool.store, &pi, at + 1, false, |_| {
        panic!("the Codex post-reset read is already available")
    })
    .unwrap();
    assert!(
        seen.error.is_none(),
        "same-second post-reset Codex reading leaves pi on {:?}",
        seen.error
    );
    assert_eq!(seen.used(at + 1), Some(95.0));
}
#[test]
fn review_inflight_pi_poll_must_not_reopen_newer_backend_denial() {
    let pool = Pool::new();
    let codex = pool.add(Provider::Codex, "native", "synthetic-member-a");
    let pi = pool.add(Provider::Pi, "pi", "synthetic-member-a");
    let at = aswap::now();
    // Pi starts first and finishes last; the distinct per-program lock permits
    // the Codex read during the Pi fetch. No network or time delay is needed.
    let result = usage::cached_mode_with(&pool.store, &pi, at, true, |_| {
        let mut denied = reading(40.0);
        denied.ordinary_usage = usage::OrdinaryUsage::Denied;
        usage::cached_mode_with(&pool.store, &codex, at + 1, true, |_| Ok(denied)).unwrap();
        let mut stale = reading(10.0);
        stale.ordinary_usage = usage::OrdinaryUsage::Allowed;
        Ok(stale)
    })
    .unwrap();
    assert_eq!(pool.cache(&codex).used(at + 1), Some(100.0));
    assert_ne!(
        result.used(at + 1),
        Some(10.0),
        "older Pi response reopened an account the newer native response denies"
    );
}

#[test]
fn review_real_pi_waiter_resumes_from_a_codex_redemption() {
    use std::{
        fs,
        io::{BufRead, BufReader},
        os::unix::fs::PermissionsExt,
        process::{Command, Stdio},
        sync::{Arc, Mutex},
        time::{Duration, Instant},
    };
    struct ChildGuard(std::process::Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let pool = Pool::new();
    let codex = pool.add(Provider::Codex, "native", "synthetic-member-a");
    let pi = pool.add(Provider::Pi, "pi", "synthetic-member-a");
    let root = pool._temp.path();
    let bin = root.join("bin");
    let log = root.join("markers");
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(&log).unwrap();
    fs::create_dir_all(root.join("pi-agent")).unwrap();
    let python = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|p| p.join("python3"))
        .find(|p| p.is_file())
        .unwrap();
    let script = r#"
import datetime,json,os,pathlib,sys
home=pathlib.Path(os.environ['PI_CODING_AGENT_DIR']); log=pathlib.Path(os.environ['ASWAP_REVIEW_LOG']); args=sys.argv[1:]
id=args[args.index('--session-id')+1]
report=pathlib.Path(os.environ['ASWAP_PI_SESSION_REPORT']); report.write_text(json.dumps({'sessionId':id}))
if (log/'first').exists():
    (log/'resumed').write_text(id)
    sys.exit(0)
(log/'first').write_text(id)
sessions=home/'sessions'/'--synthetic--'; sessions.mkdir(parents=True,exist_ok=True)
now=lambda:datetime.datetime.now(datetime.timezone.utc).isoformat(timespec='milliseconds').replace('+00:00','Z')
with (sessions/('2026-11-18T00-00-00-000Z_'+id+'.jsonl')).open('w') as out:
    out.write(json.dumps({'type':'session','id':id,'timestamp':now()})+'\n')
    out.write(json.dumps({'type':'message','id':'a1','timestamp':now(),'message':{'role':'assistant','content':[],'stopReason':'error','errorMessage':'You have hit your ChatGPT usage limit (plus plan).'}})+'\n')
print('You have hit your ChatGPT usage limit',flush=True)
sys.exit(1)
"#;
    fs::write(
        bin.join("pi"),
        format!("#!{}\n{}", python.display(), script),
    )
    .unwrap();
    fs::set_permissions(bin.join("pi"), fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(bin.join("codex"), "#!/bin/sh\nexit 87\n").unwrap();
    fs::set_permissions(bin.join("codex"), fs::Permissions::from_mode(0o700)).unwrap();
    let at = aswap::now();
    usage::cached_mode_with(&pool.store, &pi, at, true, |_| Ok(reading(20.0))).unwrap();
    let path = std::env::join_paths(
        std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let mut child = ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_aswap"))
            .args(["run", "pi", "--account", "pi", "--auto"])
            .env_clear()
            .env("PATH", path)
            .env("HOME", root)
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("PI_CODING_AGENT_DIR", root.join("pi-agent"))
            .env("ASWAP_REVIEW_LOG", &log)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let trace = Arc::new(Mutex::new(String::new()));
    let copied = trace.clone();
    let stderr = child.0.stderr.take().unwrap();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let mut s = copied.lock().unwrap();
            s.push_str(&line);
            s.push('\n');
        }
    });
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if trace.lock().unwrap().contains("watching all accounts") {
            break;
        }
        assert!(Instant::now() < deadline, "{}", trace.lock().unwrap());
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "{}",
            trace.lock().unwrap()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(pool.cache(&codex).error.as_deref(), Some("provider-limit"));
    let redeemed = aswap::now() + 2;
    usage::cached_mode_with(&pool.store, &codex, redeemed, true, |_| Ok(reading(95.0))).unwrap();
    store::json_write(&aswap::reset::log_path(&pool.store),&json!({"history":[{"at":redeemed,"label":"native","outcome":"reset","reason":"synthetic"}]})).unwrap();
    aswap::reset::append(
        &pool.store,
        &aswap::reset::Event::Recovered {
            at: redeemed,
            key: "synthetic-key".into(),
            label: "native".into(),
        },
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(status.success(), "{}", trace.lock().unwrap());
            break;
        }
        assert!(
            Instant::now() < deadline,
            "pi waiter did not wake from Codex redemption: {}",
            trace.lock().unwrap()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    reader.join().unwrap();
    assert_eq!(
        fs::read_to_string(log.join("first")).unwrap(),
        fs::read_to_string(log.join("resumed")).unwrap()
    );
}

#[test]
fn review_pi_reset_mode_and_tick_cannot_redeem() {
    let pool = Pool::new();
    pool.add(Provider::Codex, "native", "synthetic-member-a");
    pool.add(Provider::Pi, "pi", "synthetic-member-a");
    let mut settings = aswap::store::Auto::default();
    assert!(
        aswap::reset::set_mode(
            &pool.store,
            Provider::Pi,
            &mut settings,
            aswap::store::AutoReset::On
        )
        .is_err()
    );
    // Even a hand-written unsupported mode cannot reach observations or redeem.
    let mut state = pool.store.load().unwrap();
    settings.auto_reset = aswap::store::AutoReset::On;
    state.auto.insert(Provider::Pi, settings);
    pool.store.save(&state).unwrap();
    assert!(
        aswap::reset::tick(&pool.store, Provider::Pi)
            .unwrap()
            .is_none()
    );
    assert!(!aswap::reset::journal_path(&pool.store).exists());
}
