use aswap::{
    provider::Provider,
    store::{Account, Store},
    usage::{self, Cached, Usage, Window},
};
fn account(name: &str) -> Account {
    Account {
        provider: Provider::Claude,
        label: name.into(),
    }
}
fn observation(p: f64, at: u64) -> Cached {
    Cached {
        usage: Usage {
            windows: vec![Window {
                name: "five_hour".into(),
                used_percent: p,
                resets_at: None,
                duration_minutes: Some(300),
            }],
        },
        fetched_at: at,
        next_poll_at: at + 180,
        ..Default::default()
    }
}
#[test]
fn cache_serializes_fetches_and_backoff_survives_processes() {
    let t = tempfile::tempdir().unwrap();
    let store = Store::open(t.path().join("store")).unwrap();
    let a = account("one");
    {
        let _guard = store.lock().unwrap();
        let mut s = store.load().unwrap();
        s.accounts.push(a.clone());
        store.save(&s).unwrap();
    }
    let c = usage::cached_with(&store, &a, 1000, |_| Ok(observation(42.0, 1000).usage)).unwrap();
    assert_eq!(c.used(1000), Some(42.0));
    usage::cached_with(&store, &a, 1179, |_| panic!("cache bypassed")).unwrap();
    let c = usage::cached_with(&store, &a, 1180, |_| {
        Err(usage::Failure {
            kind: "http-429",
            retry_after: 3600,
        })
    })
    .unwrap();
    assert!(c.next_poll_at >= 4840);
    assert_eq!(c.used(1180), None);
    usage::cached_with(&store, &a, 2000, |_| panic!("backoff bypassed")).unwrap();
    let c = usage::cached_with(&store, &a, 5000, |_| Ok(observation(10.0, 5000).usage)).unwrap();
    assert_eq!(c.used(5000), Some(10.0));
}
#[test]
fn normalized_provider_usage_and_unknown_values() {
    let u = usage::parse_claude(
        &serde_json::json!({"five_hour":{"utilization":95},"seven_day":{"utilization":20}}),
    )
    .unwrap();
    assert_eq!(u.used(1), Some(95.0));
    assert!(usage::parse_claude(&serde_json::json!({"five_hour":{"utilization":-1}})).is_err());
    let u = usage::parse_codex(&serde_json::json!({"rateLimitsByLimitId":{"codex":{"primary":{"usedPercent":33,"resetsAt":2000,"windowDurationMins":300},"secondary":{"usedPercent":50}}}})).unwrap();
    assert_eq!(u.used(1000), Some(50.0));
    assert_eq!(u.used(2000), None);
    assert_eq!(
        usage::parse_codex(&serde_json::json!({"rateLimits":null}))
            .unwrap()
            .used(1),
        None
    );
}
#[test]
fn selection_cooldown_hysteresis_and_stale_safety() {
    let entries = vec![
        (account("a"), observation(91.0, 1000)),
        (account("b"), observation(70.0, 1000)),
        (account("c"), observation(30.0, 1000)),
    ];
    assert_eq!(
        usage::choose("a", &entries, 90.0, 0, 1000, false)
            .unwrap()
            .label,
        "c"
    );
    assert!(usage::choose("a", &entries, 90.0, 900, 1000, false).is_none());
    assert!(usage::choose("a", &entries, 90.0, 0, 1700, false).is_none());
    assert!(usage::choose("b", &entries, 90.0, 0, 1000, false).is_none());
    assert_eq!(
        usage::choose("b", &entries, 90.0, 900, 1000, true)
            .unwrap()
            .label,
        "c"
    );
}

#[test]
fn concurrent_collectors_share_one_fetch() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("store");
    let store = Store::open(root.clone()).unwrap();
    let a = account("shared");
    {
        let _guard = store.lock().unwrap();
        let mut state = store.load().unwrap();
        state.accounts.push(a.clone());
        store.save(&state).unwrap();
    }
    let count = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..8 {
            let root = &root;
            let count = &count;
            let a = &a;
            scope.spawn(move || {
                let store = Store::open(root.clone()).unwrap();
                usage::cached_with(&store, a, 1000, |_| {
                    count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(observation(20.0, 1000).usage)
                })
                .unwrap();
            });
        }
    });
    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
}
#[test]
fn app_server_handshake_and_redaction() {
    use std::os::unix::fs::PermissionsExt;
    let t = tempfile::tempdir().unwrap();
    let bin = t.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let fake = bin.join("codex");
    std::fs::write(&fake, r##"#!/bin/sh
set -eu
[ "$1" = '-c' ]
shift 2
[ "$1" = 'app-server' ]
IFS= read -r initialize
case "$initialize" in *'"initialize"'*) ;; *) exit 4;; esac
printf '%s\n' '{"id":1,"result":{}}'
IFS= read -r initialized
case "$initialized" in *'"initialized"'*) ;; *) exit 5;; esac
IFS= read -r limits
case "$limits" in *'account/rateLimits/read'*) ;; *) exit 6;; esac
if [ -f "$HOME/fail" ]; then
  printf '%s\n' '{"id":2,"error":{"message":"synthetic-private-error"}}'
else
  printf '%s\n' '{"method":"noise","params":{}}' '{"id":2,"result":{"rateLimits":{"primary":{"usedPercent":27,"windowDurationMins":300}}}}'
fi
"##).unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = std::env::join_paths(
        std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "rpc_child", "--nocapture"])
        .env_clear()
        .env("PATH", path)
        .env("HOME", t.path())
        .env("CODEX_HOME", t.path().join("codex"))
        .env("CLAUDE_CONFIG_DIR", t.path().join("claude"))
        .env("XDG_DATA_HOME", t.path().join("data"))
        .env("ASWAP_RPC_CHILD", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains("synthetic-private-error"));
}
#[test]
fn rpc_child() {
    if std::env::var("ASWAP_RPC_CHILD").as_deref() != Ok("1") {
        return;
    }
    let profile = Provider::Codex.current_home().unwrap();
    let v = usage::codex_rpc(&profile).unwrap();
    assert_eq!(usage::parse_codex(&v).unwrap().used(1), Some(27.0));
    std::fs::write(
        std::path::PathBuf::from(std::env::var_os("HOME").unwrap()).join("fail"),
        b"synthetic",
    )
    .unwrap();
    let err = usage::codex_rpc(&profile).unwrap_err();
    assert!(!format!("{err:?} {err}").contains("synthetic-private-error"));
}
