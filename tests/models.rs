use aswap::{
    provider::Provider,
    store::{self, Account, Store},
    usage::{Cached, Usage, Window},
};
use serde_json::json;
use std::{fs, os::unix::fs::PermissionsExt, process::Command};
const ID: &str = "11111111-2222-4333-8444-555555555555";

fn scenario(mode: &str, source: &str) -> std::process::Output {
    let t = tempfile::tempdir().unwrap();
    let home = t.path().join("home");
    let codex = home.join(".codex");
    fs::create_dir_all(&codex).unwrap();
    fs::write(codex.join("config.toml"), "model = 'synthetic-sol'\n").unwrap();
    if source == "profile" {
        fs::write(codex.join("config.toml"), "model = 'synthetic-other'\n").unwrap();
        fs::write(codex.join("work.config.toml"), "model = 'synthetic-sol'\n").unwrap();
    }
    let store = Store::open(t.path().join("data/aswap")).unwrap();
    let mut state = store.load().unwrap();
    for (label, percent) in [("one", 20.), ("two", 1.), ("three", 10.)] {
        let a = Account {
            provider: Provider::Codex,
            label: label.into(),
        };
        let p = store.profile(a.provider, label).unwrap();
        store::json_write(
            &p.join("auth.json"),
            &json!({"tokens":{"access_token":format!("synthetic-{label}")}}),
        )
        .unwrap();
        store::json_write(
            &p.join("usage-cache.json"),
            &Cached {
                fetched_at: aswap::now(),
                next_poll_at: aswap::now() + 3600,
                usage: Usage {
                    windows: vec![Window {
                        name: "weekly".into(),
                        used_percent: percent,
                        resets_at: None,
                        duration_minutes: Some(10080),
                    }],
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();
        state.accounts.push(a);
    }
    state.active.insert(Provider::Codex, "one".into());
    store.save(&state).unwrap();
    let fake = t.path().join("codex");
    let python = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|p| p.join("python3"))
        .find(|p| p.is_file())
        .unwrap();
    fs::write(&fake,format!("#!{}\n{}",python.display(),r#"
import os,pathlib,json,sys,time
p=pathlib.Path(os.environ['CODEX_HOME'])
args=sys.argv[1:]
mode=os.environ['ASWAP_TEST_MODE']
auth=(p/'auth.json').read_text()
label=next(s for s in ['one','two','three'] if 'synthetic-'+s in auth)
if 'app-server' in args:
    assert not (p/'models_cache.json').exists()
    for line in sys.stdin:
        req=json.loads(line);method=req.get('method')
        if method=='initialize': result={}
        elif method=='config/read':
            assert 'synthetic-sol' in (p/'config.toml').read_text()
            result={'config':{'model':'synthetic-sol'}}
        elif method=='model/list':
            assert req['params']['includeHidden']
            if mode=='unknown' and label=='three':
                print(json.dumps({'id':req['id'],'error':{'message':'synthetic private response'}}),flush=True);continue
            allowed=label=='three' and mode not in ['none','runtime-rejected']
            # A catalog can be stale; explicit provider rejection overrides it.
            allowed=allowed or (mode=='runtime-rejected' and label=='two')
            if allowed and (not req['params'].get('cursor') or mode=='loop'):
                result={'data':[{'id':'other','model':'other'}],'nextCursor':'page2'}
            else:
                result={'data':[{'id':'synthetic-sol' if allowed else 'other','model':'synthetic-sol' if allowed else 'other'}]}
        else: continue
        print(json.dumps({'id':req['id'],'result':result}),flush=True)
    sys.exit(0)
with open(os.environ['ASWAP_TEST_LOG'],'a') as log:log.write(label+'\n')
assert any('synthetic-sol' in arg for arg in args)
if label=='three':
    assert args[args.index('resume')+1]=='11111111-2222-4333-8444-555555555555'
    sys.exit(0)
if label=='two':
    assert mode=='runtime-rejected'
    print('The synthetic-sol model is not supported when using Codex with a ChatGPT account',flush=True)
    time.sleep(.5);sys.exit(1)
print('usage_limit_reached',flush=True)
time.sleep(.5);sys.exit(1)
"#)).unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
    let log = t.path().join("launches");
    let mut command = Command::new(env!("CARGO_BIN_EXE_aswap"));
    command.args([
        "run",
        "codex",
        "--auto",
        "--account",
        "one",
        "--",
        "resume",
        ID,
    ]);
    match source {
        "joined" => {
            command.arg("-msynthetic-sol");
        }
        "short" => {
            command.args(["-m", "synthetic-sol"]);
        }
        "long" => {
            command.args(["--model", "synthetic-sol"]);
        }
        "override" => {
            command.args(["-c", "model=\"synthetic-sol\""]);
        }
        "profile" => {
            command.args(["--profile", "work"]);
        }
        "config" => {}
        _ => {
            command.args(["--model=synthetic-sol"]);
        }
    }
    let output = command
        .env_clear()
        .env("HOME", &home)
        .env("CODEX_HOME", &codex)
        .env("XDG_DATA_HOME", t.path().join("data"))
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("ASWAP_CODEX_BIN", &fake)
        .env("ASWAP_TEST_LOG", &log)
        .env("ASWAP_TEST_MODE", mode)
        .output()
        .unwrap();
    if mode == "runtime-rejected" {
        let cache: Cached = store::json_read(
            &store
                .profile(Provider::Codex, "two")
                .unwrap()
                .join("usage-cache.json"),
        )
        .unwrap();
        assert!(
            cache.error.is_none(),
            "model rejection must not spend the account quota"
        );
    }
    let launches = fs::read_to_string(log).unwrap();
    if mode == "ok" {
        assert_eq!(launches, "one\nthree\n");
    } else if mode == "runtime-rejected" {
        assert_eq!(launches, "one\ntwo\n");
    } else {
        assert_eq!(launches, "one\n");
    }
    output
}
#[test]
fn incompatible_best_quota_is_skipped_and_exact_session_model_preserved() {
    let out = scenario("ok", "inline");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("skipping codex:two"));
}
#[test]
fn effective_config_model_is_resolved_and_pinned_across_switches() {
    let out = scenario("ok", "config");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
#[test]
fn unavailable_or_unverifiable_model_never_launches_and_explains_no_fit() {
    for mode in ["none", "unknown", "loop", "runtime-rejected"] {
        let out = scenario(mode, "inline");
        assert!(!out.status.success());
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            text.contains("no eligible account with verified support for model synthetic-sol"),
            "{text}"
        );
        assert!(!text.contains("synthetic private response"));
    }
}

#[test]
fn model_flags_overrides_and_selected_profile_are_preserved() {
    for source in ["short", "joined", "long", "override", "profile"] {
        let out = scenario("ok", source);
        assert!(
            out.status.success(),
            "{source}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
