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
    let fake = r##"import json, os, pathlib, sys, time
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
if args == ['--continue'] or args == ['resume', '--last']:
    assert (p / 'synthetic-history').read_text() == 'synthetic conversation'
    auth = (p / ('.credentials.json' if pathlib.Path(sys.argv[0]).name == 'claude' else 'auth.json')).read_text()
    assert 'synthetic-second' in auth
    (p / 'resumed').write_text('synthetic')
    sys.exit(0)
assert args == []
(p / 'synthetic-history').write_text('synthetic conversation')
print('usage_limit_reached', file=sys.stderr, flush=True)
sys.exit(1)
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
        let sessions = fs::read_dir(store.root.join("sessions"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect::<Vec<_>>();
        let session = sessions
            .iter()
            .find(|p| p.join(provider.auth_file()).exists())
            .unwrap();
        assert!(session.join("resumed").exists());
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
