use aswap::{
    provider::{self, Provider},
    session::AutoArgs,
    store::{self, Store},
    usage::{Cached, Usage, Window},
};
use std::{fs, os::unix::fs::PermissionsExt, process::Command};
const ID: &str = "11111111-2222-4333-8444-555555555555";
#[test]
fn provider_processes_share_state_preserve_cwd_and_resume_exact_session() {
    let t = tempfile::tempdir().unwrap();
    let bin = t.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let fake = r##"import json, os, pathlib, subprocess, sys, time
provider=pathlib.Path(sys.argv[0]).name
p=pathlib.Path(os.environ['CLAUDE_CONFIG_DIR'] if provider=='claude' else os.environ['CODEX_HOME'])
args=sys.argv[1:]
if args[:1]==['-c']: args=args[2:]
assert os.getcwd()==os.environ['ASWAP_EXPECT_CWD']
assert 'ANTHROPIC_API_KEY' not in os.environ and 'OPENAI_API_KEY' not in os.environ
assert (p/'projects' if provider=='claude' else p/'sessions').is_symlink()
(p/'private-child-file').write_text('synthetic')
if args==['--test-exit']: time.sleep(.1);sys.exit(7)
assert os.isatty(0) and os.isatty(1)
authfile=p/('.credentials.json' if provider=='claude' else 'auth.json')
auth=authfile.read_text()
id='11111111-2222-4333-8444-555555555555'
base=['--resume',id] if provider=='claude' else ['resume',id]
sticky=['--model','synthetic-model']
transcript=p/'projects'/'-synthetic'/(id+'.jsonl') if provider=='claude' else p/'sessions'/'2026'/'01'/'01'/('rollout-synthetic-'+id+'.jsonl')
transcript.parent.mkdir(parents=True,exist_ok=True)
if 'synthetic-second' in auth:
    expected=base+sticky
    assert args==expected or args==expected+['--','You were interrupted by a usage-limit account switch. Continue exactly where you left off.']
    assert transcript.read_text()=='synthetic shared conversation'
    (p/'resume-args').write_text(json.dumps(args))
    for label in ['one','two']:
        result=subprocess.run([os.environ['ASWAP_TEST_BINARY'],'rename',provider,label,'must-not-rename','--json'],capture_output=True)
        assert result.returncode!=0
    sys.exit(0)
assert args==base+sticky
transcript.write_text('synthetic shared conversation')
if provider=='claude': (p/'sessions'/(str(os.getpid())+'.json')).write_text(json.dumps({'pid':os.getpid(),'sessionId':id}))
print('usage_limit_reached',file=sys.stderr,flush=True)
if 'synthetic-clean' in auth: sys.exit(0)
with transcript.open('a') as held:
    time.sleep(30)
"##;
    let python = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|p| p.join("python3"))
        .find(|p| p.is_file())
        .unwrap();
    for name in ["claude", "codex"] {
        let path = bin.join(name);
        fs::write(&path, format!("#!{}\n{fake}", python.display())).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let path = std::env::join_paths(
        std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let cwd = t.path().join("actual-project");
    fs::create_dir(&cwd).unwrap();
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "runner_child", "--nocapture"])
        .current_dir(&cwd)
        .env_clear()
        .env("PATH", path)
        .env("HOME", t.path())
        .env("XDG_DATA_HOME", t.path().join("data"))
        .env("CLAUDE_CONFIG_DIR", t.path().join("claude"))
        .env("CODEX_HOME", t.path().join("codex"))
        .env("ASWAP_RUNNER_CHILD", "1")
        .env("ASWAP_TEST_BINARY", env!("CARGO_BIN_EXE_aswap"))
        .env("ASWAP_EXPECT_CWD", &cwd)
        .env("OPENAI_API_KEY", "synthetic-override")
        .env("ANTHROPIC_API_KEY", "synthetic-override")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
fn auth(provider: Provider, token: &str) -> serde_json::Value {
    if provider == Provider::Claude {
        serde_json::json!({"claudeAiOauth":{"accessToken":token}})
    } else {
        serde_json::json!({"tokens":{"access_token":token}})
    }
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
            store::json_write(&source.join(provider.auth_file()), &auth(provider, token)).unwrap();
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
                    )
                });
            }
        });
        let initial = if provider == Provider::Claude {
            vec![
                "--resume".into(),
                ID.into(),
                "--model".into(),
                "synthetic-model".into(),
            ]
        } else {
            vec![
                "resume".into(),
                ID.into(),
                "--model".into(),
                "synthetic-model".into(),
            ]
        };
        for prompt in [store::default_resume_prompt(), None] {
            let mut state = store.load().unwrap();
            state.auto.entry(provider).or_default().resume_prompt = prompt.clone();
            store.save(&state).unwrap();
            let cache = store
                .profile(provider, "one")
                .unwrap()
                .join("usage-cache.json");
            let mut c: Cached = store::json_read(&cache).unwrap();
            c.error = None;
            c.next_poll_at = aswap::now() + 3600;
            store::json_write(&cache, &c).unwrap();
            assert_eq!(
                aswap::runner::run(&store, provider, Some("one"), &initial, true).unwrap(),
                0
            );
            let views = fs::read_dir(
                store
                    .root
                    .join("runtimes")
                    .join(provider.as_str())
                    .join("one"),
            )
            .unwrap();
            let expected = AutoArgs::parse(provider, &initial)
                .unwrap()
                .resume(provider, ID, prompt.as_deref())
                .unwrap();
            assert!(
                views
                    .map(|e| e.unwrap().path().join("resume-args"))
                    .filter(|p| p.exists())
                    .any(
                        |p| serde_json::from_slice::<Vec<String>>(&fs::read(p).unwrap()).unwrap()
                            == expected
                    )
            );
        }
        let cache = store
            .profile(provider, "one")
            .unwrap()
            .join("usage-cache.json");
        let mut c: Cached = store::json_read(&cache).unwrap();
        c.error = None;
        store::json_write(&cache, &c).unwrap();
        store::json_write(
            &store
                .profile(provider, "one")
                .unwrap()
                .join(provider.auth_file()),
            &auth(provider, "synthetic-clean"),
        )
        .unwrap();
        assert_eq!(
            aswap::runner::run(&store, provider, Some("one"), &initial, true).unwrap(),
            0
        );
    }
}
#[test]
fn auto_pass_through_and_literal_resume_arguments() {
    for provider in [Provider::Claude, Provider::Codex] {
        let args = AutoArgs::parse(provider, &[]).unwrap();
        let prompt = "--synthetic $(not-a-shell) \"text\"\nnext line";
        let on = args.resume(provider, ID, Some(prompt)).unwrap();
        let off = args.resume(provider, ID, None).unwrap();
        assert_eq!(on.last().unwrap(), prompt);
        assert_eq!(on[on.len() - 2], "--");
        assert_eq!(on.len(), off.len() + 2);
        assert!(on.contains(&ID.to_string()));
        let mixed: Vec<String> = if provider == Provider::Claude {
            [
                "--resume",
                ID,
                "--dangerously-skip-permissions",
                "--permission-mode",
                "acceptEdits",
                "--model",
                "synthetic",
                "--add-dir",
                "/synthetic/one",
                "/synthetic/two",
                "--mcp-config",
                "/synthetic/mcp",
                "--print",
                "--",
                "synthetic one-shot prompt",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect()
        } else {
            [
                "resume",
                ID,
                "--model",
                "synthetic",
                "--dangerously-bypass-approvals-and-sandbox",
                "synthetic one-shot prompt",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect()
        };
        let parsed = AutoArgs::parse(provider, &mixed).unwrap();
        assert_eq!(parsed.initial, mixed);
        let restart = parsed.resume(provider, ID, None).unwrap();
        assert!(!restart.contains(&"synthetic one-shot prompt".into()));
        assert!(!restart.contains(&"--print".into()));
        assert!(restart.contains(&"--model".into()));
        if provider == Provider::Claude {
            assert!(restart.contains(&"--dangerously-skip-permissions".into()));
            assert!(restart.contains(&"/synthetic/two".into()));
        }
        let unknown = vec!["--future-option".into(), "synthetic-value".into()];
        assert!(
            AutoArgs::parse(provider, &unknown)
                .unwrap()
                .sticky
                .ends_with(&unknown)
        );
    }
    for flag in ["--resume", "-r"] {
        assert!(
            AutoArgs::parse(
                Provider::Claude,
                &[flag.into(), ID.into(), "--model=synthetic".into()]
            )
            .is_ok()
        );
    }
    for flag in ["--continue", "-c"] {
        assert!(AutoArgs::parse(Provider::Claude, &[flag.into()]).is_ok());
    }
    assert!(
        AutoArgs::parse(
            Provider::Codex,
            &[
                "resume".into(),
                "--last".into(),
                "-m".into(),
                "synthetic".into()
            ]
        )
        .is_ok()
    );
    assert!(AutoArgs::parse(Provider::Codex, &["-c".into(), "auth=synthetic".into()]).is_ok());
}

#[test]
fn future_option_help_describes_values_without_restricting_flags() {
    let args = vec![
        "--future-bool".into(),
        "synthetic prompt".into(),
        "--future-value".into(),
        "synthetic option".into(),
    ];
    let parsed = AutoArgs::parse_with_help(
        Provider::Claude,
        &args,
        "  --future-bool        toggle\n  --future-value <value>   value",
    )
    .unwrap();
    let resumed = parsed.resume(Provider::Claude, ID, None).unwrap();
    assert!(resumed.contains(&"--future-bool".into()));
    assert!(resumed.contains(&"synthetic option".into()));
    assert!(!resumed.contains(&"synthetic prompt".into()));
    let selectors = vec![
        "--session-id".into(),
        ID.into(),
        "--fork-session".into(),
        "--permission-mode=acceptEdits".into(),
    ];
    let parsed = AutoArgs::parse(Provider::Claude, &selectors).unwrap();
    assert_eq!(parsed.initial, selectors);
    let resumed = parsed.resume(Provider::Claude, ID, None).unwrap();
    assert!(!resumed.contains(&"--session-id".into()));
    assert!(!resumed.contains(&"--fork-session".into()));
    assert!(resumed.contains(&"--permission-mode=acceptEdits".into()));
}
