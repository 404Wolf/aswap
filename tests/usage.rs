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
            status: Some(429),
            kind: "http-429",
            retry_after: 3600,
        })
    })
    .unwrap();
    assert!(c.next_poll_at >= 4780);
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
  printf '%s\n' '{"id":2,"error":{"message":"synthetic-private-error","data":{"http_status":401}}}'
else
  case "$CODEX_HOME" in */other) used=73 ;; *) used=27 ;; esac
  printf '%s\n' '{"method":"noise","params":{}}'
  printf '{"id":2,"result":{"rateLimits":{"primary":{"usedPercent":%s,"windowDurationMins":300}}}}\n' "$used"
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
    let other = profile.join("other");
    std::fs::create_dir_all(&other).unwrap();
    let v = usage::codex_rpc(&other).unwrap();
    assert_eq!(usage::parse_codex(&v).unwrap().used(1), Some(73.0));
    std::fs::write(
        std::path::PathBuf::from(std::env::var_os("HOME").unwrap()).join("fail"),
        b"synthetic",
    )
    .unwrap();
    let err = usage::codex_rpc(&profile).unwrap_err();
    assert!(!format!("{err:?} {err}").contains("synthetic-private-error"));
    aswap::store::json_write(
        &profile.join("auth.json"),
        &serde_json::json!({"tokens":{"access_token":"synthetic-access"}}),
    )
    .unwrap();
    let failure = usage::fetch(
        &Account {
            provider: Provider::Codex,
            label: "synthetic".into(),
        },
        &profile,
    )
    .unwrap_err();
    assert_eq!(failure.status, Some(401));
    assert_eq!(failure.kind, "http-401");
    assert!(!format!("{failure:?}").contains("synthetic-private-error"));
}

#[test]
fn percentages_are_percentage_points_and_windows_remain_separate() {
    for p in [0.0, 0.25, 1.0, 99.0, 100.0] {
        let u = usage::parse_claude(&serde_json::json!({"five_hour":{"utilization":p}, "seven_day":{"utilization":99},"seven_day_opus":{"utilization":12.5}})).unwrap();
        assert_eq!(u.windows[0].used_percent, p);
        assert_eq!(u.windows[1].used_percent, 99.0);
        assert_eq!(u.windows[2].used_percent, 12.5);
        let u =
            usage::parse_codex(&serde_json::json!({"rateLimits":{"primary":{"usedPercent":p}}}))
                .unwrap();
        assert_eq!(u.windows[0].used_percent, p);
    }
    for bad in [
        serde_json::json!(-1),
        serde_json::json!(100.01),
        serde_json::json!("99"),
        serde_json::Value::Null,
    ] {
        assert!(
            usage::parse_claude(&serde_json::json!({"five_hour":{"utilization":bad}})).is_err()
        );
        assert!(
            usage::parse_codex(&serde_json::json!({"rateLimits":{"primary":{"usedPercent":bad}}}))
                .is_err()
        );
    }
}

#[test]
fn expired_windows_refetch_once_then_respect_failure_backoff_and_manual_refresh() {
    let t = tempfile::tempdir().unwrap();
    let store = Store::open(t.path().join("store")).unwrap();
    let a = account("one");
    let mut state = store.load().unwrap();
    state.accounts.push(a.clone());
    store.save(&state).unwrap();
    let mut old = observation(70.0, 1000);
    old.next_poll_at = 9000;
    old.usage.windows[0].resets_at = Some(1100);
    aswap::store::json_write(
        &store
            .profile(a.provider, &a.label)
            .unwrap()
            .join("usage-cache.json"),
        &old,
    )
    .unwrap();
    let c = usage::cached_mode_with(&store, &a, 1101, false, |_| {
        Err(usage::Failure::http(429, 2000))
    })
    .unwrap();
    assert_eq!(c.last_attempt, 1101);
    assert!(c.next_poll_at >= 3101);
    assert_eq!(c.http_status, Some(429));
    usage::cached_mode_with(&store, &a, 1102, false, |_| {
        panic!("reset must not bypass Retry-After repeatedly")
    })
    .unwrap();
    let c = usage::cached_mode_with(&store, &a, 1103, true, |_| Ok(observation(5.0, 1103).usage))
        .unwrap();
    assert_eq!(c.used(1103), Some(5.0));
    assert!(c.diagnostic().is_none());
}
#[test]
fn errors_are_safe_and_backoff_caps_without_truncating_retry_after() {
    let mut cache = Cached::default();
    for n in 0..20 {
        cache = usage::updated(cache, Err(usage::Failure::http(401, 0)), 1000 + n);
        assert!(cache.next_poll_at <= 1000 + n + 900);
        assert_eq!(
            cache.diagnostic().unwrap()["class"],
            "expired-or-invalid-token"
        );
    }
    let c = usage::updated(cache, Err(usage::Failure::http(429, 7200)), 2000);
    assert_eq!(c.next_poll_at, 9200);
    assert_eq!(c.diagnostic().unwrap()["retry_after_seconds"], 7200);
    assert_eq!(usage::parse_retry_after(Some("120"), 1000), 120);
    assert_eq!(
        usage::parse_retry_after(Some("Thu, 01 Jan 1970 00:20:00 GMT"), 1000),
        200
    );
    let c = Cached {
        error: Some("synthetic-private-error-material".into()),
        ..Default::default()
    };
    assert!(
        !c.diagnostic()
            .unwrap()
            .to_string()
            .contains("synthetic-private")
    );
}
#[test]
fn legacy_unexplained_hour_long_backoff_is_not_preserved() {
    let t = tempfile::tempdir().unwrap();
    let store = Store::open(t.path().join("store")).unwrap();
    let a = account("one");
    let mut state = store.load().unwrap();
    state.accounts.push(a.clone());
    store.save(&state).unwrap();
    let old = Cached {
        next_poll_at: 99000,
        error: Some("usage-unavailable".into()),
        fetched_at: 1000,
        ..Default::default()
    };
    aswap::store::json_write(
        &store
            .profile(a.provider, &a.label)
            .unwrap()
            .join("usage-cache.json"),
        &old,
    )
    .unwrap();
    let c = usage::cached_mode_with(
        &store,
        &a,
        2000,
        false,
        |_| Ok(observation(8.0, 2000).usage),
    )
    .unwrap();
    assert_eq!(c.used(2000), Some(8.0));
}
