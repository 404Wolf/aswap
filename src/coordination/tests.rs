use super::*;
fn seed() -> (tempfile::TempDir, Store) {
    let t = tempfile::tempdir().unwrap();
    let mut store = Store::open(t.path().join("store")).unwrap();
    store.sync_policy_path = t.path().join("sync-policy.json");
    let a = Account {
        provider: Provider::Claude,
        label: "one".into(),
    };
    let mut state = store.load().unwrap();
    state.accounts.push(a.clone());
    store.save(&state).unwrap();
    let profile = store.profile(a.provider, &a.label).unwrap();
    store::json_write(&profile.join(".credentials.json"),&json!({"claudeAiOauth":{"accessToken":"synthetic-owner-access","refreshToken":"synthetic-owner-refresh","expiresAt":9999999999999u64}})).unwrap();
    store::json_write(&profile.join(".claude.json"),&json!({"oauthAccount":{"accountUuid":"synthetic-owner"},"projects":{"synthetic-private-project":true}})).unwrap();
    let cached = Cached {
        usage: usage::Usage {
            windows: vec![usage::Window {
                name: "five_hour".into(),
                used_percent: 42.0,
                resets_at: None,
                duration_minutes: Some(300),
                codex_slot: None,
            }],
            ..Default::default()
        },
        fetched_at: now(),
        next_poll_at: now() + 3600,
        ..Default::default()
    };
    store::json_write(&profile.join("usage-cache.json"), &cached).unwrap();
    owner(&store).unwrap();
    (t, store)
}
#[test]
fn owner_packets_share_one_cache_and_never_export_refresh_capability_or_project_state() {
    let (_t, store) = seed();
    let request = Request::new(1, None);
    let one = packet(&store, &request).unwrap();
    let two = packet(&store, &request).unwrap();
    assert_eq!(
        one.profiles[0].cache.fetched_at,
        two.profiles[0].cache.fetched_at
    );
    let encoded = serde_json::to_vec(&one).unwrap();
    let text = String::from_utf8(encoded.clone()).unwrap();
    assert!(!text.contains("synthetic-owner-refresh"));
    assert!(!text.contains("synthetic-private-project"));
    let t = tempfile::tempdir().unwrap();
    let peer = Store::open(t.path().join("peer")).unwrap();
    let mut cfg = Settings {
        host: Some("synthetic-owner".into()),
        ..Default::default()
    };
    assert_eq!(
        install(&peer, serde_json::from_slice(&encoded).unwrap(), &mut cfg)
            .unwrap()
            .0,
        1
    );
    let auth: Value = store::json_read(
        &peer
            .profile(Provider::Claude, "one")
            .unwrap()
            .join(".credentials.json"),
    )
    .unwrap();
    assert!(auth["claudeAiOauth"].get("refreshToken").is_none());
    assert!(!is_owner(&peer).unwrap());
    let a = Account {
        provider: Provider::Claude,
        label: "one".into(),
    };
    let cache = usage::cached_force(&peer, &a, false).unwrap();
    assert_eq!(cache.used(now()), Some(42.0));
    // Followers consume imported cached quota and never invoke HTTP/refresh.
    assert!(
        !crate::refresh::refresh_with(&peer, &a, 99999999999, true, |_, _| panic!(
            "follower must never rotate"
        ))
        .unwrap()
    );
    let mut collision = packet(&store, &request).unwrap();
    collision.profiles[0].config = json!({"oauthAccount":{"accountUuid":"synthetic-different"}});
    assert!(install(&peer, collision, &mut cfg).is_err());
}
#[test]
fn hosts_and_transport_cannot_be_injected_or_used_as_human_credential_output() {
    for bad in [
        "",
        "-oProxyCommand=synthetic",
        "synthetic;touch",
        "synthetic user",
        "user@example.com",
        "x\ncommand",
    ] {
        assert!(!valid_host(bad));
    }
    assert!(valid_host("synthetic-owner"));
    let (_t, store) = seed();
    assert!(serve(&store, false).is_err());
    assert!(serve(&store, true).is_err());
}
#[test]
fn duplicate_identities_are_aliases_but_duplicate_labels_and_identity_changes_are_conflicts() {
    let (_t, store) = seed();
    let source = store.profile(Provider::Claude, "one").unwrap();
    let target = store.profile(Provider::Claude, "alias").unwrap();
    for file in [".credentials.json", ".claude.json", "usage-cache.json"] {
        let value: Value = store::json_read(&source.join(file)).unwrap();
        store::json_write(&target.join(file), &value).unwrap();
    }
    let mut state = store.load().unwrap();
    state.accounts.push(Account {
        provider: Provider::Claude,
        label: "alias".into(),
    });
    store.save(&state).unwrap();
    let request = Request::new(1, None);
    let p = packet(&store, &request).unwrap();
    assert_eq!(duplicate_count(&p), 1);
    let peer_dir = tempfile::tempdir().unwrap();
    let peer = Store::open(peer_dir.path().join("peer")).unwrap();
    let mut config = Settings::default();
    assert_eq!(install(&peer, p, &mut config).unwrap().0, 2);
    assert_eq!(peer.load().unwrap().accounts.len(), 2);
    let mut p = packet(&store, &request).unwrap();
    p.profiles[1].account.label = p.profiles[0].account.label.clone();
    assert!(
        install(&peer, p, &mut config)
            .unwrap_err()
            .0
            .contains("label-conflict")
    );
    let mut p = packet(&store, &request).unwrap();
    p.profiles[1].config = json!({"oauthAccount":{"accountUuid":"synthetic-other"}});
    assert!(
        install(&peer, p, &mut config)
            .unwrap_err()
            .0
            .contains("identity-conflict")
    );
    // Preflight failures do not replace either alias's credentials.
    let current = crate::identity::read(
        Provider::Claude,
        &peer.profile(Provider::Claude, "alias").unwrap(),
    )
    .unwrap();
    let original = crate::identity::read(Provider::Claude, &source).unwrap();
    assert!(crate::identity::same(&current, &original));
}
#[test]
fn owner_role_and_protocol_are_distinct_and_owner_cannot_be_silently_demoted() {
    let (_t, store) = seed();
    let err = packet(&store, &Request::new(2, None)).err().unwrap();
    assert_eq!(err.0, "sync: version-mismatch");
    assert!(
        configure(&store, "synthetic-peer")
            .unwrap_err()
            .0
            .contains("owner-role-conflict")
    );
    assert!(is_owner(&store).unwrap());
    off(&store).unwrap();
    let err = packet(&store, &Request::new(1, None)).err().unwrap();
    assert!(err.0.contains("owner-not-configured"));
}
#[test]
fn first_line_filter_never_forwards_unknown_text_or_suffixes() {
    assert_eq!(
        remote_line(b"aswap: sync server requires the configured refresh owner\n"),
        "owner-not-configured"
    );
    assert_eq!(
        remote_line(b"aswap: sync: version-mismatch\r\n"),
        "version-mismatch"
    );
    for bad in [
        "synthetic-private-token alice@example.com",
        "aswap: sync server requires the configured refresh owner alice@example.com",
        "aswap: sync: version-mismatch\x1b[31m",
        "{\"access_token\":\"synthetic-private-token\"}",
        "aswap: sync: version-mismatch\nsynthetic-second-line",
    ] {
        assert_eq!(remote_line(bad.as_bytes()), "redacted-unrecognized");
    }
    assert_eq!(remote_line(&[255, 254]), "redacted-unrecognized");
}
#[test]
fn exchange_timeout_bad_response_and_stderr_drain_are_bounded() {
    let request = Request::new(1, None);
    let mut command = Command::new("sh");
    command.args(["-c", "sleep 10"]);
    let start = Instant::now();
    let error = exchange_command(command, &request, Duration::from_millis(50))
        .err()
        .unwrap();
    assert_eq!(error.0, "sync: timeout");
    assert!(start.elapsed() < Duration::from_secs(2));
    let mut command = Command::new("sh");
    command.args([
        "-c",
        "printf 'not-json'; printf 'synthetic-private-stderr' >&2",
    ]);
    let error = exchange_command(command, &request, Duration::from_secs(2))
        .err()
        .unwrap();
    assert!(error.0.contains("bad-response"));
    let mut command = Command::new("sh");
    command.args([
        "-c",
        "printf 'aswap: sync: version-mismatch\\n' >&2; head -c 100000 /dev/zero >&2; exit 1",
    ]);
    let error = exchange_command(command, &request, Duration::from_secs(2))
        .err()
        .unwrap();
    assert_eq!(error.0, "sync: version-mismatch");
}
fn follower_store() -> (tempfile::TempDir, Store) {
    let t = tempfile::tempdir().unwrap();
    let mut store = Store::open(t.path().join("follower")).unwrap();
    store.sync_policy_path = t.path().join("sync-policy.json");
    store::json_write(
        &store.root.join("coordination.json"),
        &Settings {
            host: Some("synthetic-owner".into()),
            ..Settings::default()
        },
    )
    .unwrap();
    (t, store)
}
fn owner_packet(owner: &Store) -> Packet {
    packet(owner, &Request::new(1, None)).unwrap()
}
fn policy(store: &Store, value: Value) {
    std::fs::write(&store.sync_policy_path, serde_json::to_vec(&value).unwrap()).unwrap();
}
#[test]
fn profile_policy_defaults_exact_keys_deny_precedence_and_invalid_files() {
    let (_f, store) = follower_store();
    let a = Account {
        provider: Provider::Claude,
        label: "one".into(),
    };
    assert!(
        SyncPolicy::read(&store.sync_policy_path)
            .unwrap()
            .permits(&a)
    );
    for (value, allowed) in [
        (json!({"allow":null}), true),
        (json!({"allow":[]}), false),
        (json!({"allow":["codex:one"]}), false),
        (json!({"allow":["claude:One"]}), false),
        (json!({"allow":["claude:one"]}), true),
        (json!({"allow":["claude:one"],"deny":["claude:one"]}), false),
    ] {
        policy(&store, value);
        assert_eq!(
            SyncPolicy::read(&store.sync_policy_path)
                .unwrap()
                .permits(&a),
            allowed
        );
    }
    for value in [
        json!({"allow":["one"]}),
        json!({"allow":["other:one"]}),
        json!({"deny":["claude:../one"]}),
        json!({"allow":false}),
        json!({"deny":null}),
        json!({"typo":[]}),
    ] {
        policy(&store, value);
        assert_eq!(
            SyncPolicy::read(&store.sync_policy_path).err().unwrap().0,
            "sync: invalid-profile-policy"
        );
    }
    std::fs::write(&store.sync_policy_path, b"{").unwrap();
    assert!(SyncPolicy::read(&store.sync_policy_path).is_err());
    std::fs::write(&store.sync_policy_path, vec![b' '; 65537]).unwrap();
    assert!(SyncPolicy::read(&store.sync_policy_path).is_err());
    std::fs::remove_file(&store.sync_policy_path).unwrap();
    std::os::unix::fs::symlink(store.root.join("missing"), &store.sync_policy_path).unwrap();
    assert!(SyncPolicy::read(&store.sync_policy_path).is_err());
}
#[test]
fn policy_filters_before_auth_checks_and_reload_applies_to_stale_packets() {
    let (_o, owner) = seed();
    let (_f, store) = follower_store();
    policy(&store, json!({"allow":[]}));
    let mut packet = owner_packet(&owner);
    packet.profiles[0].auth = json!({"invalid":"synthetic"});
    assert_eq!(accept(&store, packet).unwrap().count, 0);
    assert!(store.load().unwrap().accounts.is_empty());
    assert!(!store.profile(Provider::Claude, "one").unwrap().exists());
    // Role configuration does not own/overwrite the policy file.
    off(&store).unwrap();
    assert!(
        !SyncPolicy::read(&store.sync_policy_path)
            .unwrap()
            .permits(&owner.load().unwrap().accounts[0])
    );
    policy(&store, json!({"allow":["claude:one"]}));
    assert_eq!(accept(&store, owner_packet(&owner)).unwrap().count, 1);
    let before = std::fs::read(store.root.join("state.json")).unwrap();
    let auth = store
        .profile(Provider::Claude, "one")
        .unwrap()
        .join(".credentials.json");
    let old_auth = std::fs::read(&auth).unwrap();
    policy(&store, json!({"allow":[]}));
    let mut stale = owner_packet(&owner);
    stale.issued_at = settings(&store).unwrap().last_issued - 1;
    assert!(
        accept(&store, stale)
            .err()
            .unwrap()
            .0
            .starts_with("sync: excluded-local-profile")
    );
    assert_eq!(
        std::fs::read(store.root.join("state.json")).unwrap(),
        before
    );
    assert_eq!(std::fs::read(&auth).unwrap(), old_auth);
}
#[test]
fn policy_rejects_existing_idle_active_and_busy_profiles_without_partial_install() {
    let (_o, owner) = seed();
    for busy in [false, true] {
        let (_f, store) = follower_store();
        accept(&store, owner_packet(&owner)).unwrap();
        let a = store.load().unwrap().accounts[0].clone();
        let lease = busy.then(|| store.lease(&a).unwrap());
        let before = std::fs::read(store.root.join("state.json")).unwrap();
        let auth = store
            .profile(a.provider, &a.label)
            .unwrap()
            .join(a.provider.auth_file());
        let bytes = std::fs::read(&auth).unwrap();
        policy(&store, json!({"deny":["claude:one"]}));
        let mut reply = owner_packet(&owner);
        reply.profiles[0].account.label = "two".into();
        assert!(
            accept(&store, reply)
                .err()
                .unwrap()
                .0
                .starts_with("sync: excluded-local-profile")
        );
        assert_eq!(
            std::fs::read(store.root.join("state.json")).unwrap(),
            before
        );
        assert_eq!(std::fs::read(&auth).unwrap(), bytes);
        assert!(!store.profile(Provider::Claude, "two").unwrap().exists());
        if busy {
            assert!(provider::remove(&store, a.provider, &a.label).is_err());
        }
        drop(lease);
        provider::remove(&store, a.provider, &a.label).unwrap();
        assert_eq!(accept(&store, owner_packet(&owner)).unwrap().count, 0);
    }
}
#[test]
fn poll_and_watch_both_enforce_the_declarative_policy() {
    let (_o, owner) = seed();
    let (f, store) = follower_store();
    policy(&store, json!({"allow":[]}));
    assert_eq!(
        poll_with(&store, None, true, |_, _| Ok(owner_packet(&owner))).unwrap(),
        0
    );
    let reply = f.path().join("reply.json");
    std::fs::write(
        &reply,
        format!(
            "{}\n",
            serde_json::to_string(&owner_packet(&owner)).unwrap()
        ),
    )
    .unwrap();
    let watch = || {
        watch_session(
            &store,
            |_| {
                let mut c = Command::new("sh");
                c.args([
                    "-c",
                    &format!("head -n 1 >/dev/null; cat '{}'", reply.display()),
                ]);
                Ok(c)
            },
            Duration::from_secs(3),
            Duration::from_secs(3),
        )
    };
    assert_eq!(watch().unwrap(), Some(1));
    assert!(store.load().unwrap().accounts.is_empty());
    // An atomic Home Manager symlink is supported and reread next packet.
    let managed = f.path().join("managed.json");
    std::fs::write(&managed, br#"{"allow":["claude:one"]}"#).unwrap();
    std::fs::remove_file(&store.sync_policy_path).unwrap();
    std::os::unix::fs::symlink(&managed, &store.sync_policy_path).unwrap();
    let mut cfg = settings(&store).unwrap();
    cfg.watch_next = 0;
    store::json_write(&store.root.join("coordination.json"), &cfg).unwrap();
    assert_eq!(watch().unwrap(), Some(1));
    assert_eq!(store.load().unwrap().accounts.len(), 1);
}
#[test]
fn a_slow_exchange_never_holds_the_store_lock_or_starts_twice() {
    let (_o, owner) = seed();
    let (_f, store) = follower_store();
    let reply = owner_packet(&owner);
    let (started_tx, started) = std::sync::mpsc::channel();
    let (release, release_rx) = std::sync::mpsc::channel::<()>();
    let slow = {
        let store = Store::open(store.root.clone()).unwrap();
        std::thread::spawn(move || {
            poll_with(&store, None, false, move |host, _| {
                assert_eq!(host, "synthetic-owner");
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(reply)
            })
        })
    };
    started.recv().unwrap();
    // Another launch takes the store lock at once while the exchange runs,
    let start = Instant::now();
    drop(store.lock().unwrap());
    assert!(start.elapsed() < Duration::from_secs(1));
    // and does not start a second exchange of its own.
    assert_eq!(
        poll_with(&store, None, false, |_, _| panic!("second exchange")).unwrap(),
        0
    );
    release.send(()).unwrap();
    assert_eq!(slow.join().unwrap().unwrap(), 1);
    let after = settings(&store).unwrap();
    assert!(after.last_sync > 0 && after.failures == 0);
    assert_eq!(store.load().unwrap().accounts.len(), 1);
}
#[test]
fn a_failed_exchange_records_backoff_so_peers_do_not_retry() {
    let (_f, store) = follower_store();
    let error = poll_with(&store, None, false, |_, _| Err(Error("sync: timeout")))
        .err()
        .unwrap();
    assert_eq!(error.0, "sync: timeout");
    let after = settings(&store).unwrap();
    assert_eq!(after.failures, 1);
    assert!(after.last_attempt > 0 && after.last_sync == 0);
    // One failure retries 30s later; never the old 120-300s. Measured
    // from the recorded attempt, not a clock that may have ticked since.
    let attempt = after.last_attempt;
    assert!(!due(&after, attempt));
    assert!(!due(&after, attempt + 29));
    assert!(due(&after, attempt + 30));
    assert_eq!(
        poll_with(&store, None, false, |_, _| panic!("retried inside backoff")).unwrap(),
        0
    );
    // An explicit sync still goes through, and success clears the backoff.
    let (_o, owner) = seed();
    let reply = owner_packet(&owner);
    assert_eq!(poll_with(&store, None, true, |_, _| Ok(reply)).unwrap(), 1);
    let after = settings(&store).unwrap();
    assert_eq!(after.failures, 0);
    assert!(!due(&after, now()));
    assert!(due(&after, now() + 31));
}
#[test]
fn a_follower_watch_installs_a_second_packet_pushed_after_the_first() {
    let (o, owner) = seed();
    let (f, store) = follower_store();
    let profile = owner.profile(Provider::Claude, "one").unwrap();
    let rotate = |token: &str| {
        store::json_write(&profile.join(".credentials.json"),&json!({"claudeAiOauth":{"accessToken":token,"refreshToken":"synthetic-owner-refresh","expiresAt":9999999999999u64}})).unwrap();
    };
    // The owner notices its own rotation from file stamps alone.
    let before = auth_print(&owner);
    rotate("synthetic-first-access");
    assert!(auth_print(&owner) != before);
    let first = o.path().join("first.json");
    std::fs::write(
        &first,
        [
            serde_json::to_vec(&owner_packet(&owner)).unwrap(),
            b"\n".to_vec(),
        ]
        .concat(),
    )
    .unwrap();
    rotate("synthetic-second-access");
    let second = o.path().join("second.json");
    std::fs::write(
        &second,
        [
            serde_json::to_vec(&owner_packet(&owner)).unwrap(),
            b"\n".to_vec(),
        ]
        .concat(),
    )
    .unwrap();
    let installed = store
        .profile(Provider::Claude, "one")
        .unwrap()
        .join(".credentials.json");
    let marker = f.path().join("request.json");
    // A fake owner: read the request line, push the first packet, wait
    // until the follower installed it, then push the rotation on the
    // same open stream.
    let script = format!(
        "head -n 1 > '{m}'; cat '{a}'; i=0; until grep -q synthetic-first-access '{p}' 2>/dev/null; do i=$((i+1)); [ $i -gt 200 ] && exit 3; sleep 0.02; done; cat '{b}'",
        m = marker.display(),
        a = first.display(),
        b = second.display(),
        p = installed.display(),
    );
    let start = Instant::now();
    let accepted = watch_session(
        &store,
        |host| {
            assert_eq!(host, "synthetic-owner");
            let mut command = Command::new("sh");
            command.args(["-c", &script]);
            Ok(command)
        },
        Duration::from_secs(5),
        Duration::from_secs(5),
    )
    .unwrap();
    assert_eq!(accepted, Some(2));
    assert!(start.elapsed() < Duration::from_secs(3));
    let request: Value = serde_json::from_slice(&std::fs::read(&marker).unwrap()).unwrap();
    assert_eq!(request["protocol"], WATCH_PROTOCOL);
    let auth: Value = store::json_read(&installed).unwrap();
    assert_eq!(
        auth["claudeAiOauth"]["accessToken"],
        "synthetic-second-access"
    );
    assert!(auth["claudeAiOauth"].get("refreshToken").is_none());
    // A stream that delivered and then ended is not a failure.
    let after = settings(&store).unwrap();
    assert_eq!(after.failures, 0);
    assert!(after.last_sync >= after.last_attempt);
    // Only one process per store streams.
    let held = store.watch_lock().unwrap().unwrap();
    assert!(store.watch_lock().unwrap().is_none());
    watch(&store).unwrap();
    drop(held);
}
#[test]
fn a_failed_or_hanging_watch_backs_off_itself_but_never_the_poll() {
    let (_f, store) = follower_store();
    let poll_state = |s: &Settings| (s.last_sync, s.last_attempt, s.failures);
    let before = settings(&store).unwrap();
    let fail = |script: &'static str, first: Duration| {
        watch_session(
            &store,
            move |_| {
                let mut command = Command::new("sh");
                command.args(["-c", script]);
                Ok(command)
            },
            first,
            Duration::from_secs(5),
        )
        .unwrap_err()
    };
    // An owner that refuses the stream. It reads the request first, as the
    // real owner does; exiting before that races the write (EPIPE).
    let error = fail(
        "head -c 1 >/dev/null; printf 'aswap: sync: version-mismatch\\n' >&2; exit 1",
        Duration::from_secs(5),
    );
    assert_eq!(error.0, "sync: version-mismatch");
    let after = settings(&store).unwrap();
    assert_eq!(after.watch_failures, 1);
    assert!(after.watch_next >= now() + 110);
    assert!(poll_state(&after) == poll_state(&before));
    for at in [now(), now() + 31, now() + 300] {
        assert_eq!(due(&after, at), due(&before, at));
    }
    assert_eq!(
        watch_session(
            &store,
            |_| panic!("reconnected inside backoff"),
            Duration::from_secs(5),
            Duration::from_secs(5)
        )
        .unwrap(),
        None
    );
    // An older owner that waits for stdin EOF and never sends a packet:
    // give up quickly and leave it alone for ten minutes.
    let mut s = settings(&store).unwrap();
    s.watch_next = 0;
    store::json_write(&store.root.join("coordination.json"), &s).unwrap();
    let start = Instant::now();
    let error = fail("cat > /dev/null", Duration::from_millis(300));
    assert_eq!(error.0, NO_STREAM);
    assert!(start.elapsed() < Duration::from_secs(3));
    let after = settings(&store).unwrap();
    assert!(after.watch_next >= now() + 590);
    assert!(poll_state(&after) == poll_state(&before));
    for at in [now(), now() + 31, now() + 300] {
        assert_eq!(due(&after, at), due(&before, at));
    }
    // The poll fallback still runs on its own schedule.
    let (_o, owner) = seed();
    let reply = owner_packet(&owner);
    assert_eq!(poll_with(&store, None, false, |_, _| Ok(reply)).unwrap(), 1);
}
#[test]
fn an_unchanged_heartbeat_leaves_installed_files_alone() {
    let (_o, owner) = seed();
    let (_f, store) = follower_store();
    let profile = store.profile(Provider::Claude, "one").unwrap();
    let first = owner_packet(&owner);
    {
        let _lock = store.lock().unwrap();
        accept(&store, first).unwrap();
    }
    let auth = crate::shared::stamp(&profile.join(".credentials.json"));
    let config = crate::shared::stamp(&profile.join(".claude.json"));
    let state = crate::shared::stamp(&store.root.join("state.json"));
    assert!(auth.is_some());
    let mut heartbeat = owner_packet(&owner);
    heartbeat.issued_at += 20;
    let heartbeat_issued = heartbeat.issued_at;
    {
        let _lock = store.lock().unwrap();
        accept(&store, heartbeat).unwrap();
    }
    assert!(crate::shared::stamp(&profile.join(".credentials.json")) == auth);
    assert!(crate::shared::stamp(&profile.join(".claude.json")) == config);
    assert!(crate::shared::stamp(&store.root.join("state.json")) == state);
    let after = settings(&store).unwrap();
    assert_eq!(after.last_issued, heartbeat_issued);
    assert!(after.last_sync > 0);
    // A changed token is still written.
    store::json_write(&owner.profile(Provider::Claude, "one").unwrap().join(".credentials.json"),&json!({"claudeAiOauth":{"accessToken":"synthetic-rotated","refreshToken":"synthetic-owner-refresh","expiresAt":9999999999999u64}})).unwrap();
    install(&store, owner_packet(&owner), &mut settings(&store).unwrap()).unwrap();
    assert!(crate::shared::stamp(&profile.join(".credentials.json")) != auth);
}
#[test]
fn a_follower_hands_an_installed_rotation_to_its_live_views() {
    let (_o, owner) = seed();
    let (_f, store) = follower_store();
    install(&store, owner_packet(&owner), &mut settings(&store).unwrap()).unwrap();
    let a = Account {
        provider: Provider::Claude,
        label: "one".into(),
    };
    // A running view of the account, as View::prepare leaves it.
    let view = store.root.join("runtimes/claude/one/run-synthetic");
    let profile = store.profile(Provider::Claude, "one").unwrap();
    let installed: Value = store::json_read(&profile.join(".credentials.json")).unwrap();
    store::json_write(
        &view.join(".credentials.json"),
        &crate::refresh::launch_auth(Provider::Claude, &installed),
    )
    .unwrap();
    std::fs::write(view.with_extension("lock"), "one").unwrap();
    let lease = store.lease(&a).unwrap();
    lease.view(&view).unwrap();
    store::json_write(&owner.profile(Provider::Claude, "one").unwrap().join(".credentials.json"),&json!({"claudeAiOauth":{"accessToken":"synthetic-rotated","refreshToken":"synthetic-owner-refresh","expiresAt":9999999999999u64}})).unwrap();
    let mut packet = owner_packet(&owner);
    packet.issued_at += 1;
    {
        let _lock = store.lock().unwrap();
        accept(&store, packet).unwrap();
    }
    let handed: Value = store::json_read(&view.join(".credentials.json")).unwrap();
    assert_eq!(handed["claudeAiOauth"]["accessToken"], "synthetic-rotated");
    assert_eq!(
        handed["claudeAiOauth"]["refreshToken"],
        "aswap-owner-managed"
    );
}
#[test]
fn a_follower_never_refreshes_even_for_its_own_running_session() {
    let (_o, owner) = seed();
    let (_f, store) = follower_store();
    install(&store, owner_packet(&owner), &mut settings(&store).unwrap()).unwrap();
    let a = Account {
        provider: Provider::Claude,
        label: "one".into(),
    };
    // Expired, forced and as an active run: still no refresh request.
    for active in [false, true] {
        let _lock = store.lock().unwrap();
        assert!(
            !crate::refresh::refresh_inner(&store, &a, u64::MAX / 2, true, active, |_, _| {
                panic!("a follower must never rotate")
            })
            .unwrap()
        );
    }
    let auth: Value = store::json_read(
        &store
            .profile(Provider::Claude, "one")
            .unwrap()
            .join(".credentials.json"),
    )
    .unwrap();
    assert!(auth["claudeAiOauth"].get("refreshToken").is_none());
    // Its launch views carry only the public owner-managed sentinel.
    let launch = crate::refresh::launch_auth(Provider::Claude, &auth);
    assert_eq!(
        launch["claudeAiOauth"]["refreshToken"],
        "aswap-owner-managed"
    );
}

/// coordination.json as an older release writes it: no watch_* fields,
/// and its own failure count after repeated failures.
fn old_writer(store: &Store, last_sync: u64, last_attempt: u64, failures: u32) {
    store::json_write(
        &store.root.join("coordination.json"),
        &json!({"owner":false,"host":"synthetic-owner","last_sync":last_sync,
            "last_attempt":last_attempt,"failures":failures,"last_issued":0}),
    )
    .unwrap();
}
#[test]
fn the_poll_backoff_is_capped_at_a_minute_whatever_older_releases_wrote() {
    let at = now();
    let s = |last_sync, last_attempt, failures| Settings {
        host: Some("synthetic-owner".into()),
        last_sync,
        last_attempt,
        failures,
        ..Default::default()
    };
    // Delivered 30s ago: due.
    assert!(due(&s(at - 30, at - 30, 0), at));
    assert!(!due(&s(at - 29, at - 29, 0), at));
    // In flight (claimed, unfinished): someone else retries after 60s.
    assert!(!due(&s(at - 600, at - 59, 0), at));
    assert!(due(&s(at - 600, at - 60, 0), at));
    // Failed once: 30s; failed many times (an old writer's count): 60s.
    assert!(due(&s(at - 600, at - 30, 1), at));
    assert!(!due(&s(at - 600, at - 59, 9), at));
    assert!(due(&s(at - 600, at - 60, u32::MAX), at));
    // A clock stepped back never postpones the poll.
    assert!(due(&s(at + 3600, at + 3600, 0), at));
    assert!(due(&s(0, at + 3600, 2), at));
    // An old writer dropped the watch fields: still readable, same rule.
    let (_f, store) = follower_store();
    old_writer(&store, at - 600, at - 61, 9);
    let read = settings(&store).unwrap();
    assert_eq!((read.watch_next, read.watch_failures), (0, 0));
    assert!(due(&read, at));
}
#[test]
fn the_fallback_poller_polls_with_no_stream_and_an_old_writers_backoff() {
    let (_o, owner) = seed();
    let (_f, store) = follower_store();
    // The stream is down and an old release recorded nine failures; its
    // own rule would wait 300s. Last delivery and attempt were 61s ago.
    old_writer(&store, now() - 120, now() - 61, 9);
    let reply = std::cell::RefCell::new(Some(owner_packet(&owner)));
    let mut calls = 0;
    poll_loop(&store, Duration::from_millis(10), || {
        calls += 1;
        let result = poll_with(&store, None, false, |_, _| {
            Ok(reply.borrow_mut().take().expect("one exchange"))
        });
        if calls == 3 {
            // Stop the loop: it runs only while this store is a follower.
            off(&store).unwrap();
        }
        result
    });
    assert_eq!(calls, 3);
    // The first tick polled and installed; the next ones were not due.
    assert!(reply.borrow().is_none());
    assert_eq!(store.load().unwrap().accounts.len(), 1);
    let events = recent_events(&store, 10);
    let names: Vec<_> = events
        .iter()
        .map(|e| (e["kind"].as_str().unwrap(), e["event"].as_str().unwrap()))
        .collect();
    assert_eq!(names, [("poll", "claim"), ("poll", "installed")]);
}
#[test]
fn a_stale_packet_counts_as_contact_but_a_clock_step_back_still_installs() {
    let (_o, owner) = seed();
    let (_f, store) = follower_store();
    let first = owner_packet(&owner);
    let issued = first.issued_at;
    {
        let _lock = store.lock().unwrap();
        accept(&store, first).unwrap();
    }
    let profile = store.profile(Provider::Claude, "one").unwrap();
    let rotate = |token: &str| {
        store::json_write(&owner.profile(Provider::Claude, "one").unwrap().join(".credentials.json"),&json!({"claudeAiOauth":{"accessToken":token,"refreshToken":"synthetic-owner-refresh","expiresAt":9999999999999u64}})).unwrap();
    };
    // A slower overlapping exchange: skipped, yet the poll is not left
    // looking in flight (which would wait out the in-flight backoff).
    rotate("synthetic-overlap");
    let mut slow = owner_packet(&owner);
    slow.issued_at = issued - 10;
    let mut s = settings(&store).unwrap();
    s.last_sync = 0;
    s.last_attempt = now();
    store::json_write(&store.root.join("coordination.json"), &s).unwrap();
    let done = {
        let _lock = store.lock().unwrap();
        accept(&store, slow).unwrap()
    };
    assert_eq!(done.event, "stale");
    let after = settings(&store).unwrap();
    assert!(after.last_sync >= after.last_attempt);
    assert_eq!(after.last_issued, issued);
    let auth: Value = store::json_read(&profile.join(".credentials.json")).unwrap();
    assert_eq!(
        auth["claudeAiOauth"]["accessToken"],
        "synthetic-owner-access"
    );
    // The owner's clock stepped back by an hour: install, don't skip
    // every packet until its clock catches up.
    rotate("synthetic-after-clock-step");
    let mut stepped = owner_packet(&owner);
    stepped.issued_at = issued - 3600;
    let done = {
        let _lock = store.lock().unwrap();
        accept(&store, stepped).unwrap()
    };
    assert_eq!(done.event, "installed");
    let auth: Value = store::json_read(&profile.join(".credentials.json")).unwrap();
    assert_eq!(
        auth["claudeAiOauth"]["accessToken"],
        "synthetic-after-clock-step"
    );
}
#[test]
fn the_sync_log_is_private_secret_free_capped_and_shown_in_status() {
    use std::os::unix::fs::PermissionsExt;
    let (_o, owner) = seed();
    let (_f, store) = follower_store();
    poll_with(&store, None, true, |_, _| {
        Err(Error("sync: timeout; synthetic detail"))
    })
    .unwrap_err();
    let reply = owner_packet(&owner);
    poll_with(&store, None, true, |_, _| Ok(reply)).unwrap();
    // A heartbeat that changes nothing is not logged by the stream, and
    // a poll reports it as unchanged.
    let reply = owner_packet(&owner);
    poll_with(&store, None, true, |_, _| Ok(reply)).unwrap();
    let path = store.root.join(LOG);
    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    let text = std::fs::read_to_string(&path).unwrap();
    for secret in [
        "synthetic-owner-access",
        "synthetic-owner-refresh",
        "synthetic-owner\"",
        "synthetic detail",
        "accessToken",
    ] {
        assert!(!text.contains(secret), "{secret} leaked into the sync log");
    }
    let events = recent_events(&store, 10);
    let names: Vec<_> = events
        .iter()
        .map(|e| (e["event"].as_str().unwrap(), e["code"].as_str()))
        .collect();
    assert_eq!(
        names,
        [
            ("claim", None),
            ("fail", Some("timeout")),
            ("claim", None),
            ("installed", None),
            ("claim", None),
            ("unchanged", None),
        ]
    );
    let fail = &events[1];
    assert_eq!(fail["failures"], 1);
    assert_eq!(fail["pid"], std::process::id());
    assert!(fail["next_poll"].as_u64().unwrap() >= fail["last_attempt"].as_u64().unwrap() + 30);
    let status = status(&store).unwrap();
    assert_eq!(status["events"].as_array().unwrap().len(), 6);
    assert!(status["next_poll"].as_u64().is_some());
    // Rotated at the size cap; the previous file is kept once.
    let filler = json!({"event":"x".repeat(1000)});
    for _ in 0..1100 {
        append_log(&path, &filler).unwrap();
    }
    assert!(std::fs::metadata(&path).unwrap().len() < LOG_LIMIT + 2048);
    assert!(store.root.join("sync.log.1").exists());
    // Stream events are logged with their own kind.
    let mut s = settings(&store).unwrap();
    s.watch_next = 0;
    store::json_write(&store.root.join("coordination.json"), &s).unwrap();
    watch_session(
        &store,
        |_| {
            let mut command = Command::new("sh");
            command.args([
                "-c",
                "printf 'aswap: sync: version-mismatch\\n' >&2; exit 1",
            ]);
            Ok(command)
        },
        Duration::from_secs(5),
        Duration::from_secs(5),
    )
    .unwrap_err();
    let events = recent_events(&store, 2);
    assert_eq!(events[0]["kind"], "stream");
    assert_eq!(events[0]["event"], "connect");
    assert_eq!(events[1]["event"], "fail");
    assert_eq!(events[1]["code"], "version-mismatch");
    assert!(events[1]["watch_next"].as_u64().unwrap() > now());
}
