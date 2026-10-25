use aswap::{
    provider::Provider,
    rename,
    store::{self, Account, Store},
    usage,
};
use serde_json::json;
fn seed() -> (tempfile::TempDir, Store) {
    let t = tempfile::tempdir().unwrap();
    let store = Store::open(t.path().join("store")).unwrap();
    let mut state = store.load().unwrap();
    for provider in [Provider::Claude, Provider::Codex] {
        state.accounts.push(Account {
            provider,
            label: "old".into(),
        });
        state.active.insert(provider, "old".into());
        let policy = state.auto.entry(provider).or_default();
        policy.enabled = true;
        policy.threshold = 84.0;
        policy.last_switch = 123;
        policy.resume_prompt = Some("synthetic follow-up".into());
        let profile = store.profile(provider, "old").unwrap();
        store::atomic(
            &profile.join("synthetic-history"),
            b"synthetic profile history",
        )
        .unwrap();
        store::json_write(&profile.join("usage-cache.json"), &json!({"usage":{"windows":[{"name":"five_hour","used_percent":42,"duration_minutes":300,"resets_at":null}]},"fetched_at":1000,"next_poll_at":2000,"failures":0,"error":null})).unwrap();
        store::atomic(
            &store
                .sessions(provider, "old")
                .unwrap()
                .join("run-synthetic/history"),
            b"synthetic retained session",
        )
        .unwrap();
    }
    store.save(&state).unwrap();
    (t, store)
}
#[test]
fn rename_active_preserves_policy_cache_and_all_history() {
    let (_t, store) = seed();
    for provider in [Provider::Claude, Provider::Codex] {
        let policy = serde_json::to_value(&store.load().unwrap().auto[&provider]).unwrap();
        let renamed = rename::rename(&store, provider, "old", "new").unwrap();
        let state = store.load().unwrap();
        assert_eq!(state.active[&provider], "new");
        assert_eq!(
            serde_json::to_value(&state.auto[&provider]).unwrap(),
            policy
        );
        assert_eq!(store.account(&state, provider, None).unwrap().label, "new");
        assert!(store.account(&state, provider, Some("old")).is_err());
        assert!(!store.profile(provider, "old").unwrap().exists());
        assert!(
            store
                .profile(provider, "new")
                .unwrap()
                .join("synthetic-history")
                .exists()
        );
        assert!(!store.sessions(provider, "old").unwrap().exists());
        assert!(
            store
                .sessions(provider, "new")
                .unwrap()
                .join("run-synthetic/history")
                .exists()
        );
        let cache = usage::cached_with(&store, &renamed, 1100, |_| {
            panic!("cache should move with the profile")
        })
        .unwrap();
        assert_eq!(cache.used(1100), Some(42.0));
    }
}
#[test]
fn running_collisions_invalid_labels_and_symlinks_are_refused() {
    let (_t, store) = seed();
    let account = Account {
        provider: Provider::Claude,
        label: "old".into(),
    };
    let lease = store.lease(&account).unwrap();
    assert!(rename::rename(&store, Provider::Claude, "old", "new").is_err());
    drop(lease);
    for invalid in ["", "../escape", "bad/name", "name@example.com"] {
        assert!(rename::rename(&store, Provider::Claude, "old", invalid).is_err());
    }
    for path in [
        store.profile(Provider::Claude, "new").unwrap(),
        store.sessions(Provider::Claude, "new").unwrap(),
    ] {
        store::private_dir(&path).unwrap();
        assert!(rename::rename(&store, Provider::Claude, "old", "new").is_err());
        std::fs::remove_dir(path).unwrap();
    }
    let mut state = store.load().unwrap();
    state.accounts.push(Account {
        provider: Provider::Claude,
        label: "collision".into(),
    });
    store.save(&state).unwrap();
    assert!(rename::rename(&store, Provider::Claude, "old", "collision").is_err());
    assert!(rename::rename(&store, Provider::Claude, "old", "old").is_err());
    std::os::unix::fs::symlink(
        store.profile(Provider::Claude, "old").unwrap(),
        store.profile(Provider::Claude, "link").unwrap(),
    )
    .unwrap();
    assert!(rename::rename(&store, Provider::Claude, "old", "link").is_err());
    assert_eq!(store.load().unwrap().active[&Provider::Claude], "old");
    assert!(store.profile(Provider::Claude, "old").unwrap().exists());
}
#[test]
fn interrupted_rename_is_completed_before_next_reader() {
    for phase in 0..4 {
        let (_t, store) = seed();
        store::json_write(
            &store.root.join("rename-transaction.json"),
            &json!({"provider":"claude","old":"old","new":"new"}),
        )
        .unwrap();
        if phase >= 1 {
            std::fs::rename(
                store.profile(Provider::Claude, "old").unwrap(),
                store.profile(Provider::Claude, "new").unwrap(),
            )
            .unwrap();
        }
        if phase >= 2 {
            std::fs::rename(
                store.sessions(Provider::Claude, "old").unwrap(),
                store.sessions(Provider::Claude, "new").unwrap(),
            )
            .unwrap();
        }
        if phase >= 3 {
            let mut state = store.load().unwrap();
            state
                .accounts
                .iter_mut()
                .find(|a| a.provider == Provider::Claude)
                .unwrap()
                .label = "new".into();
            state.active.insert(Provider::Claude, "new".into());
            store.save(&state).unwrap();
        }
        let _guard = store.lock().unwrap();
        assert_eq!(store.load().unwrap().active[&Provider::Claude], "new");
        assert!(
            store
                .sessions(Provider::Claude, "new")
                .unwrap()
                .join("run-synthetic/history")
                .exists()
        );
        assert!(!store.root.join("rename-transaction.json").exists());
    }
}
