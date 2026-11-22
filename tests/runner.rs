use aswap::{
    provider::{self, Provider},
    session::AutoArgs,
    store::{self, Store},
    usage::{Cached, Usage, Window},
};
use std::{fs, os::unix::fs::PermissionsExt, process::Command};
const ID: &str = "11111111-2222-4333-8444-555555555555";
#[test]
fn provider_wrappers_are_told_the_launch_is_already_supervised() {
    for provider in [Provider::Claude, Provider::Codex] {
        let command = provider.command(std::path::Path::new("/synthetic-view"));
        assert!(command.get_envs().any(|(key, value)| {
            key == provider.bin_var() && value == Some(command.get_program())
        }));
    }
}
#[test]
fn provider_processes_share_state_preserve_cwd_and_resume_exact_session() {
    runner_case("usage_limit_reached");
}
#[test]
fn expired_login_switches_and_resumes_exact_session() {
    runner_case("Login expired · Please run /login");
}
fn runner_case(failure: &str) {
    diagnostic_case(failure, "");
}

#[test]
fn codex_usage_limit_reloads_rotated_auth_only_when_quota_is_available() {
    let notice = "\x1b[31m■ Error running remote compact task: You've hit your usage limit. Visit https://chatgpt.com/settings/usage to purchase more credits or try again at Nov 21st, 2026 4:31 PM.\x1b[0m";
    diagnostic_case(notice, "codex-rotated");
    diagnostic_case(notice, "codex-exhausted");
}

#[test]
fn a_redrawn_codex_notice_over_a_healthy_account_never_switches() {
    // The resumed Codex shows an old limit notice at once; its account has
    // 20% used (cached, and again when read at the end of the window).
    diagnostic_case(
        "\u{25a0} You've hit your usage limit. Try again later.",
        "codex-redraw",
    );
}

#[test]
fn claude_screen_only_limit_requires_an_active_menu_and_exhausted_quota_at_startup() {
    let menu = "│ What do you want to do?\n│ ❯ 1. Stop and wait for limit to reset\n│   2. Upgrade";
    diagnostic_case(menu, "claude-menu");
    diagnostic_case(menu, "claude-healthy-menu");
    diagnostic_case(
        "⎿ Prompt is too long · automatic compaction failed: You've hit your weekly limit · resets Nov 22, 9am (America/New_York) · /clear to start fresh",
        "claude-startup-redraw",
    );
}

fn diagnostic_case(failure: &str, scenario: &str) {
    let t = tempfile::tempdir().unwrap();
    let bin = t.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let fake = r##"import json, os, pathlib, subprocess, sys, time
provider=pathlib.Path(sys.argv[0]).name
scenario=os.environ['ASWAP_TEST_DIAGNOSTIC']
# Launch views are removed once a run ends, so results go outside them.
results=pathlib.Path(os.environ['ASWAP_TEST_RESULTS'])
p=pathlib.Path(os.environ['CLAUDE_CONFIG_DIR'] if provider=='claude' else os.environ['CODEX_HOME'])
args=sys.argv[1:]
if args[:1]==['-c']: args=args[2:]
assert os.getcwd()==os.environ['ASWAP_EXPECT_CWD']
assert 'ANTHROPIC_API_KEY' not in os.environ and 'OPENAI_API_KEY' not in os.environ
if args[:1]==['app-server']:
    for line in sys.stdin:
        req=json.loads(line)
        if req.get('method')=='initialize': result={}
        elif req.get('method')=='model/list': result={'data':[{'model':'synthetic-model','id':'synthetic-model','isDefault':True}]}
        elif req.get('method')=='account/rateLimits/read': result={'rateLimits':{'primary':{'usedPercent':20,'windowDurationMins':300,'resetsAt':int(time.time())+3600}}}
        else: continue
        print(json.dumps({'id':req['id'],'result':result}),flush=True)
    sys.exit(0)
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
if 'synthetic-second' in auth or 'synthetic-rotated' in auth:
    if scenario=='codex-rotated': assert 'synthetic-rotated' in auth
    if scenario=='codex-exhausted': assert 'synthetic-second' in auth
    assert scenario not in ('claude-healthy-menu','claude-startup-redraw','codex-redraw')
    expected=base+sticky
    assert args==expected or args==expected+['--','You were interrupted by a usage-limit account switch. Continue exactly where you left off.']
    assert transcript.read_text().startswith('synthetic shared conversation')
    results.joinpath('resume-args-'+str(os.getpid())).write_text(json.dumps(args))
    for label in (['one'] if scenario=='codex-rotated' else ['one','two']):
        result=subprocess.run([os.environ['ASWAP_TEST_BINARY'],'rename',provider,label,'must-not-rename','--json'],capture_output=True)
        assert result.returncode!=0
    sys.exit(0)
assert args==base+sticky
transcript.write_text('synthetic shared conversation')
if provider=='claude': (p/'sessions'/(str(os.getpid())+'.json')).write_text(json.dumps({'pid':os.getpid(),'sessionId':id}))
profile=pathlib.Path(os.environ['XDG_DATA_HOME'])/'aswap'/'profiles'/provider/'one'
def replace_json(path, data):
    replacement=path.with_suffix('.synthetic-replacement')
    replacement.write_text(json.dumps(data))
    replacement.replace(path)
if scenario in ('codex-rotated','codex-exhausted') and 'synthetic-first' in auth:
    data=json.loads((profile/'auth.json').read_text())
    data['tokens']['access_token']='synthetic-rotated'
    replace_json(profile/'auth.json',data)
# A real Codex limit: its quota reads exhausted (an early notice needs that).
codex_limit=provider=='codex' and not scenario and 'limit' in os.environ['ASWAP_TEST_FAILURE']
if (codex_limit or scenario in ('codex-exhausted','claude-menu','claude-startup-redraw')) and 'synthetic-first' in auth:
    cache=json.loads((profile/'usage-cache.json').read_text())
    cache['usage']['windows']=[{'name':'weekly','used_percent':100,'duration_minutes':10080,'resets_at':int(time.time())+3600}]
    cache['fetched_at']=int(time.time())
    replace_json(profile/'usage-cache.json',cache)
print(os.environ['ASWAP_TEST_FAILURE'],file=sys.stderr,flush=True)
if provider=='claude' and not scenario and 'limit' in os.environ['ASWAP_TEST_FAILURE'] and 'synthetic-clean' not in auth:
    # Claude records each usage-limit error in the transcript as it happens.
    import datetime
    with transcript.open('a') as t:
        t.write('\n'+json.dumps({'type':'assistant','timestamp':datetime.datetime.now(datetime.timezone.utc).isoformat(),'isApiErrorMessage':True,'error':'rate_limit'})+'\n')
if 'synthetic-clean' in auth: sys.exit(0)
if scenario in ('claude-healthy-menu','claude-startup-redraw','codex-redraw'):
    # A resumed Codex redraws an old notice, then keeps working past the
    # early window (15 s) and the quota check at its end.
    time.sleep(18 if scenario=='codex-redraw' else .8)
    results.joinpath('completed-'+str(os.getpid())).write_text('provider continued')
    sys.exit(0)
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
    let results = t.path().join("results");
    fs::create_dir(&results).unwrap();
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
        .env("ASWAP_TEST_FAILURE", failure)
        .env("ASWAP_TEST_DIAGNOSTIC", scenario)
        .env(
            "ASWAP_TRACE_SELECTION",
            if scenario.is_empty() { "0" } else { "1" },
        )
        .env("ASWAP_TEST_BINARY", env!("CARGO_BIN_EXE_aswap"))
        .env("ASWAP_EXPECT_CWD", &cwd)
        .env("ASWAP_TEST_RESULTS", &results)
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
    if scenario == "codex-redraw" {
        let trace = String::from_utf8_lossy(&out.stderr);
        assert!(!trace.contains("waiting on a usage limit"), "{trace}");
    } else if scenario.starts_with("codex-") {
        let trace = String::from_utf8_lossy(&out.stderr);
        assert!(
            trace.contains("credentials_changed_since_launch=true"),
            "{trace}"
        );
        assert_eq!(
            trace.contains("resuming on the same account"),
            scenario == "codex-rotated"
        );
        assert!(!trace.contains("synthetic-rotated"), "{trace}");
    }
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
    let scenario = std::env::var("ASWAP_TEST_DIAGNOSTIC").unwrap_or_default();
    for provider in [Provider::Claude, Provider::Codex] {
        if !scenario.is_empty() && !scenario.starts_with(provider.as_str()) {
            continue;
        }
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
                        codex_slot: None,
                    }],
                    ..Default::default()
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
            c.usage.windows[0].used_percent = 20.0;
            c.next_poll_at = aswap::now() + 3600;
            store::json_write(&cache, &c).unwrap();
            if scenario.starts_with("codex-") {
                store::json_write(
                    &cache.with_file_name("auth.json"),
                    &auth(provider, "synthetic-first"),
                )
                .unwrap();
            }
            assert_eq!(
                aswap::runner::run(&store, provider, Some("one"), &initial, true).unwrap(),
                0
            );
            let views = fs::read_dir(std::env::var_os("ASWAP_TEST_RESULTS").unwrap()).unwrap();
            let expected = AutoArgs::parse(provider, &initial)
                .unwrap()
                .resume(provider, ID, prompt.as_deref())
                .unwrap();
            let files: Vec<_> = views.map(|entry| entry.unwrap().path()).collect();
            let named = |prefix: &'static str| {
                files.iter().filter(move |path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with(prefix))
                })
            };
            if matches!(
                scenario.as_str(),
                "claude-healthy-menu" | "claude-startup-redraw" | "codex-redraw"
            ) {
                assert!(named("completed-").next().is_some());
                assert!(named("resume-args-").next().is_none());
            } else {
                assert!(named("resume-args-").any(|path| {
                    serde_json::from_slice::<Vec<String>>(&fs::read(path).unwrap()).unwrap()
                        == expected
                }));
                if scenario == "codex-rotated" {
                    assert!(store::json_read::<Cached>(&cache).unwrap().error.is_none());
                }
            }
        }
        if !scenario.is_empty() {
            continue;
        }
        let cache = store
            .profile(provider, "one")
            .unwrap()
            .join("usage-cache.json");
        let mut c: Cached = store::json_read(&cache).unwrap();
        c.error = None;
        // The limit is over: the quota reads as it did before it.
        c.usage.windows[0].used_percent = 20.0;
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

#[test]
fn live_claude_is_not_stopped_for_quota_prose_or_unresumable_failure() {
    runner_safety_cases(&[
        "quota",
        "prose",
        "unknown",
        "ambiguous",
        "bounded401",
        "boundedquota",
    ]);
}

#[test]
fn above_threshold_fallback_skips_rejected_accounts_and_aliases_after_cache_replacement() {
    runner_safety_cases(&["boundedhigh401", "boundedhighquota"]);
}

/// Rotation cases whose account really expired, so the run fails over.
const FAILS_OVER: [&str; 4] = [
    "rotationfail401",
    "rotationfast401",
    "rotationexisting401",
    "rotationcap401",
];
#[test]
fn a_login_error_right_after_a_rotation_resumes_on_the_same_account() {
    // rotation401: the token rotated under the provider, so the account is
    // fine, even though the resumed provider redraws the old login error.
    // rotationredraw401: the provider writes the prompt before redrawing,
    // and the supervisor discovers it through the late-transcript path.
    // rotationlate401: the conversation is identified only after the error;
    // the run keeps retrying and resumes once it is known. rotationcap401:
    // after three same-account restarts the account counts as expired.
    // rotationfail401, rotationfast401 (failing 150ms after the resume
    // prompt) and rotationexisting401 (the same, with a transcript that
    // exists at launch): a provider started with the fresh token fails
    // too, so the account really expired and the run fails over.
    runner_safety_cases(&[
        "rotation401",
        "rotationredraw401",
        "rotationlate401",
        "rotationcap401",
        "rotationfail401",
        "rotationfast401",
        "rotationexisting401",
    ]);
}

#[test]
fn a_limit_switches_only_when_the_transcript_records_it() {
    // limitredraw: a resumed conversation that ended on a limit redraws the
    // notice; nothing new is recorded, so the run stays on its account.
    // limitlive: the notice is live and recorded: switch and resume.
    // limitdialog: recorded, but only a menu the screen parser can't read
    // is shown: switch and resume all the same.
    runner_safety_cases(&["limitredraw", "limitresumed", "limitlive", "limitdialog"]);
}

#[test]
fn an_unusable_store_never_ends_the_provider() {
    // While the provider runs, the store's lock and state can't be opened for
    // five seconds (as when the disk is full): the supervisor keeps the
    // provider running, then switches once the store is usable again.
    runner_safety_cases(&["limitstorefull"]);
}

#[test]
fn automatic_restarts_stop_after_eight_switches() {
    runner_safety_cases(&["boundedcap"]);
}

/// Quota exhaustion and the hourly cap now keep the supervisor alive after
/// an unsuccessful CLI exit. Observe that wait, then cancel our own child;
/// the launch/identity assertions below still prove the anti-flap bounds.
/// The late-identity fixture also uses stderr to order its registry write.
/// The redraw fixture uses PTY output as a barrier for transcript discovery.
fn observed_safety_output(
    mut command: Command,
    home: &std::path::Path,
    case: &str,
) -> std::process::Output {
    use std::{
        io::BufRead,
        process::Stdio,
        time::{Duration, Instant},
    };
    struct Owned(Option<std::process::Child>);
    impl Drop for Owned {
        fn drop(&mut self) {
            if let Some(child) = &mut self.0 {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
    let mut child = Owned(Some(
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    ));
    if case == "rotationredraw401" {
        let stdout = child.0.as_mut().unwrap().stdout.take().unwrap();
        let publish = home.join("publish-resume-transcript");
        let reader = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            for line in std::io::BufReader::new(stdout).lines() {
                let line = line.unwrap();
                if line.contains("⎿ Login expired · Please run /login") {
                    fs::write(&publish, "supervisor forwarded the redraw").unwrap();
                }
                bytes.extend_from_slice(line.as_bytes());
                bytes.push(b'\n');
            }
            bytes
        });
        let mut output = child.0.take().unwrap().wait_with_output().unwrap();
        output.stdout = reader.join().unwrap();
        return output;
    }
    let stderr = child.0.as_mut().unwrap().stderr.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        for line in std::io::BufReader::new(stderr).lines() {
            let line = line.unwrap();
            bytes.extend_from_slice(line.as_bytes());
            bytes.push(b'\n');
            let _ = tx.send(line);
        }
        bytes
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut waiting = false;
    let late_identity = case == "rotationlate401";
    loop {
        assert!(
            Instant::now() < deadline,
            "{case}: supervisor never reported waiting"
        );
        if let Ok(line) = rx.recv_timeout(Duration::from_millis(25)) {
            if late_identity && line.contains("keeping the current provider running") {
                fs::write(
                    home.join("register-session"),
                    "supervisor observed the missing identity",
                )
                .unwrap();
                break;
            }
            waiting |= line.contains("watching all accounts")
                || line.contains("eight-restarts-per-hour cap");
        }
        assert!(
            child.0.as_mut().unwrap().try_wait().unwrap().is_none(),
            "{case}: exhausted supervisor exited instead of waiting"
        );
        let ready = if case == "boundedcap" {
            home.join("cap-completed").exists()
        } else {
            fs::read_to_string(home.join("calls"))
                .unwrap_or_default()
                .lines()
                .count()
                == 2
        };
        if waiting && ready {
            break;
        }
    }
    if !late_identity {
        // SAFETY: this is our unreaped supervisor child, so its PID cannot be reused.
        assert_eq!(
            unsafe { libc::kill(child.0.as_ref().unwrap().id() as i32, libc::SIGTERM) },
            0
        );
    }
    let mut output = child.0.take().unwrap().wait_with_output().unwrap();
    output.stderr = reader.join().unwrap();
    output
}

fn runner_safety_cases(cases: &[&str]) {
    let t = tempfile::tempdir().unwrap();
    let python = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|p| p.join("python3"))
        .find(|p| p.is_file())
        .unwrap();
    let bin = t.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let fake = bin.join("claude");
    fs::write(&fake, format!("#!{}\n{}", python.display(), r#"
import datetime, json, os, pathlib, sys, time
root=pathlib.Path(os.environ['HOME'])
case=os.environ['ASWAP_SAFETY_CASE']
if '--help' in sys.argv: sys.exit(0)
with (root/'calls').open('a') as f: f.write(json.dumps(sys.argv[1:])+'\n')
NOTICE="You've hit your weekly limit · resets Nov 20, 3am (America/New_York)"
def now(): return datetime.datetime.now(datetime.timezone.utc).isoformat()
def limit_entry(view,at):
    # What Claude 2.1.3xx appends to the transcript when a request hits a limit.
    transcript=view/'projects'/'-synthetic'/'11111111-2222-4333-8444-555555555555.jsonl'
    transcript.parent.mkdir(parents=True,exist_ok=True)
    with transcript.open('a') as t:
        t.write(json.dumps({'type':'assistant','timestamp':at,'isApiErrorMessage':True,'error':'rate_limit','message':{'role':'assistant','content':[{'type':'text','text':NOTICE}]}})+'\n')
if case.startswith('limit'):
    view=pathlib.Path(os.environ['CLAUDE_CONFIG_DIR'])
    token=json.loads((view/'.credentials.json').read_text())['claudeAiOauth']['accessToken']
    with (root/'identities').open('a') as f: f.write(token+'\n')
    if token=='synthetic-two':
        (root/'completed').write_text('resumed provider completed its work')
        sys.exit(0)
    if case in ('limitredraw','limitresumed'):
        # The conversation ended on a limit hours ago. Resuming it redraws
        # that notice, but the account is fine and work goes on.
        # (limitresumed: the transcript already exists at launch.)
        if case=='limitredraw': limit_entry(view,'2026-01-01T14:42:46.895Z')
        print('  ⎿  '+NOTICE,flush=True)
        time.sleep(1.5)
        (root/'completed').write_text('synthetic tool completed')
        sys.exit(0)
    if case=='limitstorefull':
        # The disk fills up: the store's lock and state can't be opened.
        store=pathlib.Path(os.environ['XDG_DATA_HOME'])/'aswap'
        for name in ['store.lock','state.json']:
            if (store/name).exists(): (store/name).rename(store/(name+'.away'))
            (store/name).mkdir()
        limit_entry(view,now())
        print('  ⎿  '+NOTICE,flush=True)
        # The supervisor can't decide anything now, but must not stop this
        # provider over it. Long enough for several of its 1s lock attempts
        # to fail, including the one choosing another account.
        time.sleep(5)
        (root/'survived').write_text('provider kept running while the store was unusable')
        for name in ['store.lock','state.json']:
            (store/name).rmdir()
            if (store/(name+'.away')).exists(): (store/(name+'.away')).rename(store/name)
        time.sleep(30)
        sys.exit(1)
    # A live limit: recorded, then shown inline or only as a menu.
    limit_entry(view,now())
    if case=='limitlive': print('  ⎿  '+NOTICE,flush=True)
    else: print('│ What do you want to do?  1. Stop and wait for limit to reset  2. Upgrade │',flush=True)
    time.sleep(30)
    sys.exit(1)
if case.startswith('rotation'):
    view=pathlib.Path(os.environ['CLAUDE_CONFIG_DIR'])
    token=json.loads((view/'.credentials.json').read_text())['claudeAiOauth']['accessToken']
    with (root/'identities').open('a') as f: f.write(token+'\n')
    first=len((root/'calls').read_text().splitlines())==1
    transcript=view/'projects'/'-synthetic'/('11111111-2222-4333-8444-555555555555.jsonl')
    transcript.parent.mkdir(parents=True,exist_ok=True)
    def record():
        with transcript.open('a') as t: t.write('{"synthetic":"resume prompt"}\n')
    if first and case=='rotationexisting401':
        # The conversation already has a transcript when it is resumed.
        record()
    if not first:
        if case=='rotationredraw401':
            # Write the prompt before the redraw, but publish its path after
            # the supervisor forwards the redraw. A future fixture timestamp
            # exercises production's clamp to the time of late discovery.
            staged=transcript.with_suffix('.pending')
            staged.write_text(json.dumps({'type':'user','timestamp':'2099-01-01T00:00:00Z'})+'\n')
            print('  ⎿ Login expired · Please run /login',flush=True)
            deadline=time.monotonic()+10
            while not (root/'publish-resume-transcript').exists():
                assert time.monotonic()<deadline, 'supervisor never forwarded the redraw'
                time.sleep(.01)
            staged.replace(transcript)
        else:
            # A resumed Claude redraws the error that ended the previous
            # provider, then records the prompt and makes a live API call.
            print('  ⎿ Login expired · Please run /login',flush=True)
            time.sleep(.3)
            record()
        # A revoked token fails fast: one quick 401 and a failed refresh.
        time.sleep(.15 if case in ('rotationfast401','rotationexisting401') else 1.5)
    if token=='synthetic-two' or (case in ('rotation401','rotationredraw401','rotationlate401') and not first):
        (root/'completed').write_text('resumed provider completed its work')
        sys.exit(0)
    if token.startswith('synthetic-one') and (first or case=='rotationcap401'):
        # The owner rotates the account mid-call and the old token is revoked
        # before this provider's view has the new one.
        profile=pathlib.Path(os.environ['ASWAP_TEST_CACHE']).parent/'.credentials.json'
        data=json.loads(profile.read_text())
        data['claudeAiOauth']['accessToken']=token+('-rotated' if first else '-again')
        rotating=profile.with_name('.credentials.rotating')
        rotating.write_text(json.dumps(data))
        rotating.replace(profile)
    print('Login expired · Please run /login',flush=True)
    if case=='rotationlate401' and first:
        # Only now does the provider register its conversation.
        # Let the supervisor first observe the missing identity, regardless
        # of how long this machine takes to schedule its first tick.
        deadline=time.monotonic()+10
        while not (root/'register-session').exists():
            assert time.monotonic()<deadline, 'supervisor never checked the identity'
            time.sleep(.01)
        (view/'sessions'/(str(os.getpid())+'.json')).write_text(json.dumps({'pid':os.getpid(),'sessionId':'11111111-2222-4333-8444-555555555555'}))
        time.sleep(30)
    time.sleep(.4)
    sys.exit(1)
if case.startswith('bounded'):
    high=case.startswith('boundedhigh')
    # Simulate an owner push replacing all rejected caches with fresh eligibility.
    for cache in pathlib.Path(os.environ['ASWAP_TEST_CACHE']).parent.parent.glob('*/usage-cache.json'):
        data=json.loads(cache.read_text())
        rejected=data.get('error') is not None or data['usage']['windows'][0]['used_percent']>=100
        data['error']=None;data['http_status']=None
        pct=10
        if high: pct={'one':95,'alias':91,'two':94}[cache.parent.name]
        elif case=='boundedcap': pct=95
        data['usage']['windows'][0]['used_percent']=pct
        data['fetched_at']=int(time.time())
        data['next_poll_at']=int(time.time())+3600
        replacement=cache.with_suffix('.replacement')
        replacement.write_text(json.dumps(data))
        replacement.replace(cache)
        if high and cache.parent.name=='one' and rejected:
            (root/'replaced-rejection').write_text('rejected account cache replaced with fresh above-threshold capacity')
    auth=json.loads((pathlib.Path(os.environ['CLAUDE_CONFIG_DIR'])/'.credentials.json').read_text())
    with (root/'identities').open('a') as f: f.write(auth['claudeAiOauth']['accessToken']+'\n')
    print('⎿ OAuth token revoked · Please run /login' if case.endswith('401') else 'API Error: 429 synthetic quota failure',flush=True)
    if not case.endswith('401'): limit_entry(pathlib.Path(os.environ['CLAUDE_CONFIG_DIR']),now())
    if case=='boundedcap' and len((root/'calls').read_text().splitlines())==9:
        # At the cap the last provider must finish its active work, even though
        # another fresh, distinct account is still eligible for the fallback.
        time.sleep(1)
        (root/'cap-completed').write_text('ninth provider completed its work')
    else: time.sleep(.4)
    sys.exit(1)
elif case=='quota':
    cache=pathlib.Path(os.environ['ASWAP_TEST_CACHE'])
    data=json.loads(cache.read_text())
    data['usage']['windows'][0]['used_percent']=95
    cache.write_text(json.dumps(data))
elif case=='prose':
    print('❯ <hc from="t7 worker">All synthetic logins failed: login expired (401), HTTP 401.</hc>',flush=True)
    print('● The previous resume failed with "aswap: login expired for claude:synthetic".',flush=True)
    print('❯ You were interrupted by a usage-limit account switch. Continue exactly where you left off.',flush=True)
else:
    print('Login expired · Please run /login',flush=True)
# Simulate a still-running tool that must finish before the provider exits.
time.sleep(1)
(root/'completed').write_text('synthetic tool completed')
"#)).unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
    let path = std::env::join_paths(
        std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    for &case in cases {
        let home = t.path().join(case);
        fs::create_dir(&home).unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "runner_safety_child", "--nocapture"])
            .env_clear()
            .env("PATH", &path)
            .env("HOME", &home)
            .env("XDG_DATA_HOME", home.join("data"))
            .env("CLAUDE_CONFIG_DIR", home.join("claude"))
            .env("ASWAP_SAFETY_CASE", case);
        let out = if (case.starts_with("bounded") && !case.ends_with("401"))
            || matches!(case, "rotationlate401" | "rotationredraw401")
        {
            observed_safety_output(command, &home, case)
        } else {
            command.output().unwrap()
        };
        assert!(
            out.status.success(),
            "{case}: {}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        if case.starts_with("rotation") {
            let identities = fs::read_to_string(home.join("identities")).unwrap();
            let mut expected = vec!["synthetic-one", "synthetic-one-rotated"];
            if case == "rotationcap401" {
                expected.extend([
                    "synthetic-one-rotated-again",
                    "synthetic-one-rotated-again-again",
                ]);
            }
            if FAILS_OVER.contains(&case) {
                expected.push("synthetic-two");
            }
            assert_eq!(identities.lines().collect::<Vec<_>>(), expected, "{case}");
            let calls = fs::read_to_string(home.join("calls")).unwrap();
            assert!(
                calls
                    .lines()
                    .skip(1)
                    .all(|call| call.contains("--resume") && call.contains(ID)),
                "{case}: every restart must resume the exact conversation"
            );
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert_eq!(
                stderr.matches("resuming on the same account").count(),
                if case == "rotationcap401" { 3 } else { 1 },
                "{case}: {stderr}"
            );
            assert_eq!(
                stderr.contains("login expired for claude:one"),
                FAILS_OVER.contains(&case),
                "{case}: {stderr}"
            );
            assert_eq!(
                stderr.contains("keeping the current provider running"),
                case == "rotationlate401",
                "{case}: {stderr}"
            );
            assert!(home.join("completed").exists(), "{case}");
            continue;
        }
        if case.starts_with("limit") {
            let identities = fs::read_to_string(home.join("identities")).unwrap();
            let calls = fs::read_to_string(home.join("calls")).unwrap();
            let redraw = matches!(case, "limitredraw" | "limitresumed");
            assert_eq!(
                identities.lines().collect::<Vec<_>>(),
                if redraw {
                    vec!["synthetic-one"]
                } else {
                    vec!["synthetic-one", "synthetic-two"]
                },
                "{case}"
            );
            assert!(
                calls
                    .lines()
                    .all(|call| call.contains("--resume") && call.contains(ID)),
                "{case}: every launch must resume the exact conversation"
            );
            assert!(home.join("completed").exists(), "{case}");
            if case == "limitstorefull" {
                assert!(home.join("survived").exists());
                let stderr = String::from_utf8_lossy(&out.stderr);
                // Choosing an account failed while the store was unusable,
                // without stopping the provider; once the store is back the
                // run switches (the commit itself succeeded, so no
                // same-account fallback).
                assert!(
                    stderr.contains("choosing another account failed")
                        && stderr.contains("the provider keeps running"),
                    "{stderr}"
                );
                assert!(stderr.contains("using claude:two"), "{stderr}");
            }
            continue;
        }
        if case.starts_with("bounded") {
            let identities = fs::read_to_string(home.join("identities")).unwrap();
            let calls = fs::read_to_string(home.join("calls")).unwrap();
            if case == "boundedcap" {
                assert_eq!(calls.lines().count(), 9, "expected exactly eight restarts");
                assert_eq!(identities.lines().count(), 9);
                assert_eq!(
                    identities
                        .lines()
                        .collect::<std::collections::BTreeSet<_>>()
                        .len(),
                    9,
                    "the restart cap must be reached with distinct identities"
                );
                assert!(
                    home.join("cap-completed").exists(),
                    "ninth provider was stopped instead of completing its work"
                );
                assert_eq!(
                    String::from_utf8_lossy(&out.stderr)
                        .matches("aswap: using claude:")
                        .count(),
                    9,
                    "each launch must be reported, with no tenth launch"
                );
                continue;
            }
            assert_eq!(calls.lines().count(), 2, "{case}: unexpected restart");
            assert_eq!(
                identities.lines().collect::<Vec<_>>(),
                vec!["synthetic-one", "synthetic-two"],
                "{case}: rejected identity or alias revisited"
            );
            if case.starts_with("boundedhigh") {
                assert!(
                    home.join("replaced-rejection").exists(),
                    "{case}: must replace a previously rejected account's cache"
                );
            }
            continue;
        }
        assert!(
            home.join("completed").exists(),
            "{case}: active tool was stopped"
        );
        assert_eq!(
            fs::read_to_string(home.join("calls"))
                .unwrap()
                .lines()
                .count(),
            1,
            "{case}: unexpected restart"
        );
        if matches!(case, "unknown" | "ambiguous") {
            assert!(
                String::from_utf8_lossy(&out.stderr)
                    .contains("keeping the current provider running")
            );
        }
    }
}

#[test]
fn runner_safety_child() {
    let Ok(case) = std::env::var("ASWAP_SAFETY_CASE") else {
        return;
    };
    let provider = Provider::Claude;
    let store = Store::from_env().unwrap();
    let source = provider.current_home().unwrap();
    let bounded = case.starts_with("bounded");
    let distinct = bounded || case.starts_with("rotation") || case.starts_with("limit");
    let accounts = if case == "boundedcap" {
        [
            "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
        ]
        .into_iter()
        .map(|label| (label, 95.0))
        .collect()
    } else if case.starts_with("boundedhigh") {
        // The alias has the most headroom, but shares the rejected identity.
        // All accounts stay above the soft threshold, including after replacement.
        vec![("one", 95.0), ("alias", 91.0), ("two", 94.0)]
    } else if bounded {
        vec![("one", 20.0), ("alias", 10.0), ("two", 10.0)]
    } else {
        vec![("one", 20.0), ("two", 10.0)]
    };
    for (label, pct) in accounts {
        let identity = if case == "boundedcap" {
            label
        } else if label == "two" {
            "synthetic-two"
        } else {
            "synthetic-one"
        };
        store::json_write(
            &source.join(provider.auth_file()),
            &auth(
                provider,
                if distinct {
                    identity
                } else {
                    "synthetic-safety"
                },
            ),
        )
        .unwrap();
        if distinct {
            store::json_write(
                &source.join(".claude.json"),
                &serde_json::json!({"oauthAccount":{"accountUuid":identity}}),
            )
            .unwrap();
        }
        provider::add(&store, provider, label).unwrap();
        let cached = Cached {
            usage: Usage {
                windows: vec![Window {
                    name: "primary".into(),
                    used_percent: pct,
                    resets_at: None,
                    duration_minutes: Some(300),
                    codex_slot: None,
                }],
                ..Default::default()
            },
            fetched_at: aswap::now(),
            next_poll_at: aswap::now() + 3600,
            ..Default::default()
        };
        store::json_write(
            &store
                .profile(provider, label)
                .unwrap()
                .join("usage-cache.json"),
            &cached,
        )
        .unwrap();
    }
    if case.starts_with("boundedhigh") || case == "boundedcap" {
        let mut state = store.load().unwrap();
        state.auto.entry(provider).or_default().threshold = 90.0;
        // Eight quick restarts: the hourly cap, not the machine-wide
        // switch spacing (tests/switch_gate.rs), is under test here.
        state.switch_interval_seconds = 0;
        store.save(&state).unwrap();
    }
    if case == "limitresumed" {
        // The conversation being resumed ended on a limit hours ago.
        let dir = source.join("projects").join("-synthetic");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(format!("{ID}.jsonl")),
            "{\"type\":\"assistant\",\"timestamp\":\"2026-01-01T14:42:46.895Z\",\"isApiErrorMessage\":true,\"error\":\"rate_limit\"}\n",
        )
        .unwrap();
    }
    // SAFETY: this subprocess test has no other threads reading environment.
    unsafe {
        std::env::set_var(
            "ASWAP_TEST_CACHE",
            store
                .profile(provider, "one")
                .unwrap()
                .join("usage-cache.json"),
        );
    }
    let args = if matches!(case.as_str(), "unknown" | "rotationlate401") {
        vec!["--continue".into()]
    } else if case == "ambiguous" {
        vec![
            "--resume".into(),
            ID.into(),
            "--future-option".into(),
            "possibly-initial-input".into(),
        ]
    } else {
        vec!["--resume".into(), ID.into()]
    };
    let result = aswap::runner::run(&store, provider, Some("one"), &args, true);
    if bounded && !case.ends_with("401") {
        assert_eq!(
            result.unwrap_err().to_string(),
            "provider session interrupted"
        );
    } else if bounded {
        assert_ne!(result.unwrap(), 0);
    } else if matches!(case.as_str(), "unknown" | "ambiguous") {
        assert!(result.is_err());
    } else if case.starts_with("limit") {
        assert_eq!(result.unwrap(), 0);
        let cache: Cached = store::json_read(
            &store
                .profile(provider, "one")
                .unwrap()
                .join("usage-cache.json"),
        )
        .unwrap();
        assert_eq!(
            cache.error.as_deref(),
            (!matches!(case.as_str(), "limitredraw" | "limitresumed")).then_some("provider-limit"),
            "{case}: only a recorded limit marks the account"
        );
    } else if FAILS_OVER.contains(&case.as_str()) {
        assert_eq!(result.unwrap(), 0);
        let cache: Cached = store::json_read(
            &store
                .profile(provider, "one")
                .unwrap()
                .join("usage-cache.json"),
        )
        .unwrap();
        assert_eq!(cache.http_status, Some(401), "a fresh token failed too");
    } else {
        assert_eq!(result.unwrap(), 0);
        let cache: Cached = store::json_read(
            &store
                .profile(provider, "one")
                .unwrap()
                .join("usage-cache.json"),
        )
        .unwrap();
        assert!(
            cache.error.is_none(),
            "transcript prose must not poison login state"
        );
    }
}
#[test]
fn a_run_in_a_deleted_directory_says_so() {
    // t1x2, 2026-11-19: its worktree was removed under the shell, and
    // `aswap run codex --auto -- resume <id>` failed with "cannot resolve
    // model config directory".
    let t = tempfile::tempdir().unwrap();
    let gone = t.path().join("removed-worktree");
    for auto in [true, false] {
        fs::create_dir(&gone).unwrap();
        let script = format!(
            "cd '{dir}' && rmdir '{dir}' && exec '{aswap}' run codex {flag} -- resume {ID}",
            dir = gone.display(),
            aswap = env!("CARGO_BIN_EXE_aswap"),
            flag = if auto { "--auto" } else { "" },
        );
        let out = Command::new("sh")
            .args(["-c", &script])
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("HOME", t.path())
            .env("XDG_DATA_HOME", t.path().join("data"))
            .env("CODEX_HOME", t.path().join("codex"))
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(1), "{stderr}");
        assert!(
            stderr.contains("the current directory no longer exists"),
            "{stderr}"
        );
    }
}
