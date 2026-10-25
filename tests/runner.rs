use aswap::{
    provider::{self, Provider},
    store::{self, Store},
    usage::{Cached, Usage, Window},
};
use std::{fs, os::unix::fs::PermissionsExt, process::Command};

#[test]
fn fake_provider_parallel_sessions_and_limit_resume() {
    let t = tempfile::tempdir().unwrap();
    let bin = t.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let fake = r##"import json, os, pathlib, subprocess, sys, time
p = pathlib.Path(os.environ['CLAUDE_CONFIG_DIR'] if pathlib.Path(sys.argv[0]).name == 'claude' else os.environ['CODEX_HOME'])
args = sys.argv[1:]
if args[:1] == ['-c']: args = args[2:]
assert 'OPENAI_API_KEY' not in os.environ
assert 'ANTHROPIC_API_KEY' not in os.environ
(p / 'private-child-file').write_text('synthetic')
if args == ['--test-exit']:
    time.sleep(0.3)
    sys.exit(7)
assert os.isatty(0) and os.isatty(1) and os.isatty(2)
resume = ['--continue'] if pathlib.Path(sys.argv[0]).name == 'claude' else ['resume', '--last']
if args[:len(resume)] == resume:
    assert args == resume or args == resume + ['--', 'You were interrupted by a usage-limit account switch. Continue exactly where you left off.']
    (p / 'resume-args').write_text(json.dumps(args))
    assert (p / 'synthetic-history').read_text() == 'synthetic conversation'
    auth = (p / ('.credentials.json' if pathlib.Path(sys.argv[0]).name == 'claude' else 'auth.json')).read_text()
    assert 'synthetic-second' in auth
    provider = pathlib.Path(sys.argv[0]).name
    for label in ['one', 'two']:
        result = subprocess.run([os.environ['ASWAP_TEST_BINARY'], 'rename', provider, label, 'must-not-rename', '--json'], capture_output=True)
        assert result.returncode != 0, 'active/history owner must remain leased'
    (p / 'resumed').write_text('synthetic')
    sys.exit(0)
assert args == []
(p / 'synthetic-history').write_text('synthetic conversation')
print('usage_limit_reached', file=sys.stderr, flush=True)
auth = (p / ('.credentials.json' if pathlib.Path(sys.argv[0]).name == 'claude' else 'auth.json')).read_text()
sys.exit(0 if 'synthetic-clean' in auth else 1)
"##;
    for name in ["claude", "codex"] {
        let p = bin.join(name);
        let python = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|p| p.join("python3"))
            .find(|p| p.is_file())
            .expect("python3 is required for synthetic process tests");
        fs::write(&p, format!("#!{}\n{fake}", python.display())).unwrap();
        fs::set_permissions(p, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let path = std::env::join_paths(
        std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "runner_child", "--nocapture"])
        .env_clear()
        .env("PATH", path)
        .env("HOME", t.path())
        .env("XDG_DATA_HOME", t.path().join("data"))
        .env("CLAUDE_CONFIG_DIR", t.path().join("claude"))
        .env("CODEX_HOME", t.path().join("codex"))
        .env("ASWAP_RUNNER_CHILD", "1")
        .env("ASWAP_TEST_BINARY", env!("CARGO_BIN_EXE_aswap"))
        .env("OPENAI_API_KEY", "synthetic-override")
        .env("ANTHROPIC_API_KEY", "synthetic-override")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
#[test]
fn runner_child() {
    if std::env::var("ASWAP_RUNNER_CHILD").as_deref() != Ok("1") {
        return;
    }
    let store = Store::from_env().unwrap();
    for provider in [Provider::Claude, Provider::Codex] {
        let source = provider.current_home().unwrap();
        for (name, token, pct) in [
            ("one", "synthetic-first", 20.0),
            ("two", "synthetic-second", 10.0),
        ] {
            let auth = if provider == Provider::Claude {
                serde_json::json!({"claudeAiOauth":{"accessToken":token}})
            } else {
                serde_json::json!({"tokens":{"access_token":token}})
            };
            store::json_write(&source.join(provider.auth_file()), &auth).unwrap();
            provider::add(&store, provider, name).unwrap();
            let cached = Cached {
                usage: Usage {
                    windows: vec![Window {
                        name: "primary".into(),
                        used_percent: pct,
                        resets_at: None,
                        duration_minutes: Some(300),
                    }],
                },
                fetched_at: aswap::now(),
                next_poll_at: aswap::now() + 3600,
                ..Default::default()
            };
            store::json_write(
                &store
                    .profile(provider, name)
                    .unwrap()
                    .join("usage-cache.json"),
                &cached,
            )
            .unwrap();
        }
        std::thread::scope(|scope| {
            for name in ["one", "two"] {
                let store = &store;
                scope.spawn(move || {
                    assert_eq!(
                        aswap::runner::run(
                            store,
                            provider,
                            Some(name),
                            &["--test-exit".into()],
                            false
                        )
                        .unwrap(),
                        7
                    );
                });
            }
        });
        assert_eq!(
            aswap::runner::run(&store, provider, Some("one"), &[], true).unwrap(),
            0
        );
        let sessions = fs::read_dir(store.sessions(provider, "one").unwrap())
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect::<Vec<_>>();
        let session = sessions
            .iter()
            .find(|p| p.join(provider.auth_file()).exists())
            .unwrap();
        assert!(session.join("resumed").exists());
        let argv: Vec<String> =
            serde_json::from_slice(&fs::read(session.join("resume-args")).unwrap()).unwrap();
        assert_eq!(
            argv,
            aswap::runner::resume_args(provider, Some(store::RESUME_PROMPT))
        );

        assert_eq!(
            fs::metadata(session.join("private-child-file"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let auth = fs::read_to_string(
            store
                .profile(provider, "one")
                .unwrap()
                .join(provider.auth_file()),
        )
        .unwrap();
        assert!(auth.contains("synthetic-first"));

        // Disable the follow-up and verify the real subprocess argv after another limit.
        let cache_path = store
            .profile(provider, "one")
            .unwrap()
            .join("usage-cache.json");
        let mut cache: Cached = store::json_read(&cache_path).unwrap();
        cache.error = None;
        cache.fetched_at = aswap::now();
        cache.next_poll_at = aswap::now() + 3600;
        store::json_write(&cache_path, &cache).unwrap();
        let mut state = store.load().unwrap();
        state.auto.entry(provider).or_default().resume_prompt = None;
        store.save(&state).unwrap();
        assert_eq!(
            aswap::runner::run(&store, provider, Some("one"), &[], true).unwrap(),
            0
        );
        let after: Vec<_> = fs::read_dir(store.sessions(provider, "one").unwrap())
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        let off_session = after.iter().find(|p| !sessions.contains(p)).unwrap();
        let argv: Vec<String> =
            serde_json::from_slice(&fs::read(off_session.join("resume-args")).unwrap()).unwrap();
        assert_eq!(argv, aswap::runner::resume_args(provider, None));
        // A successful provider exit never sends a prompt, even with a limit-like tail.
        cache.error = None;
        store::json_write(&cache_path, &cache).unwrap();
        state.auto.entry(provider).or_default().resume_prompt = store::default_resume_prompt();
        store.save(&state).unwrap();
        let clean_auth = if provider == Provider::Claude {
            serde_json::json!({"claudeAiOauth":{"accessToken":"synthetic-clean"}})
        } else {
            serde_json::json!({"tokens":{"access_token":"synthetic-clean"}})
        };
        store::json_write(
            &store
                .profile(provider, "one")
                .unwrap()
                .join(provider.auth_file()),
            &clean_auth,
        )
        .unwrap();
        assert_eq!(
            aswap::runner::run(&store, provider, Some("one"), &[], true).unwrap(),
            0
        );
        let clean_session = fs::read_dir(store.sessions(provider, "one").unwrap())
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| !after.contains(p))
            .unwrap();
        assert!(!clean_session.join("resumed").exists());
        assert!(
            aswap::runner::run(
                &store,
                provider,
                Some("one"),
                &["unsafe-replay".into()],
                true
            )
            .is_err()
        );
    }
}

#[test]
fn resume_argv_treats_custom_prompt_as_one_literal_argument() {
    for provider in [Provider::Claude, Provider::Codex] {
        let prompt = "--synthetic $(not-a-shell) \"text\"\nnext line";
        let args = aswap::runner::resume_args(provider, Some(prompt));
        assert_eq!(args.last().unwrap(), prompt);
        assert_eq!(args[args.len() - 2], "--");
        let off = aswap::runner::resume_args(provider, None);
        assert_eq!(args.len(), off.len() + 2);
    }
}
