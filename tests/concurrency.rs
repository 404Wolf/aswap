use aswap::{
    provider::Provider,
    store::{self, Account, Store},
    usage::{Cached, Usage, Window},
};
use serde_json::json;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
struct Children(Vec<Child>);
impl Drop for Children {
    fn drop(&mut self) {
        for child in &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
fn wait_for(mut ready: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(20);
    while !ready() {
        assert!(
            Instant::now() < end,
            "synthetic sessions did not reach barrier"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}
#[test]
fn many_plain_and_auto_sessions_share_busy_accounts_without_disturbing_each_other() {
    let t = tempfile::tempdir().unwrap();
    let data = t.path().join("data");
    let store = Store::open(data.join("aswap")).unwrap();
    let home = t.path().join("claude");
    fs::create_dir(&home).unwrap();
    let ready = t.path().join("ready");
    fs::create_dir(&ready).unwrap();
    let bin = t.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let python = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|p| p.join("python3"))
        .find(|p| p.is_file())
        .unwrap();
    let fake = bin.join("claude");
    fs::write(
        &fake,
        format!(
            "#!{}\n{}",
            python.display(),
            r#"
import os,pathlib,json,sys,time
p=pathlib.Path(os.environ['CLAUDE_CONFIG_DIR'])
auth=json.loads((p/'.credentials.json').read_text())['claudeAiOauth']
assert auth['refreshToken']=='aswap-owner-managed'
args=sys.argv[1:]
id=args[args.index('--resume')+1]
ready=pathlib.Path(os.environ['ASWAP_TEST_READY'])
(ready/(id+'-'+auth['accessToken'])).write_text(str(os.getpid()))
if id.endswith('0099') and auth['accessToken']=='synthetic-one':
    print('usage_limit_reached',file=sys.stderr,flush=True)
    sys.exit(1)
while not (ready/'release').exists():time.sleep(.025)
"#
        ),
    )
    .unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
    let mut state = store.load().unwrap();
    for (label, percent) in [("one", 20.), ("two", 10.)] {
        let a = Account {
            provider: Provider::Claude,
            label: label.into(),
        };
        let p = store.profile(a.provider, label).unwrap();
        store::json_write(&p.join(".credentials.json"),&json!({"claudeAiOauth":{"accessToken":format!("synthetic-{label}"),"refreshToken":format!("synthetic-private-{label}")}})).unwrap();
        store::json_write(
            &p.join(".claude.json"),
            &json!({"oauthAccount":{"accountUuid":format!("synthetic-{label}")}}),
        )
        .unwrap();
        store::json_write(
            &p.join("usage-cache.json"),
            &Cached {
                usage: Usage {
                    windows: vec![Window {
                        name: "five_hour".into(),
                        used_percent: percent,
                        resets_at: None,
                        duration_minutes: Some(300),
                    }],
                    ..Default::default()
                },
                fetched_at: aswap::now(),
                next_poll_at: aswap::now() + 3600,
                ..Default::default()
            },
        )
        .unwrap();
        state.accounts.push(a);
    }
    state.active.insert(Provider::Claude, "one".into());
    store.save(&state).unwrap();
    let path = std::env::join_paths(
        std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let spawn = |index: u32, label: &str, auto: bool| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_aswap"));
        cmd.args(["run", "claude", "--account", label]);
        if auto {
            cmd.arg("--auto");
        }
        cmd.args([
            "--",
            "--resume",
            &format!("00000000-0000-4000-8000-{index:012}"),
        ]);
        cmd.env_clear()
            .env("HOME", t.path())
            .env("XDG_DATA_HOME", &data)
            .env("CLAUDE_CONFIG_DIR", &home)
            .env("PATH", &path)
            .env("ASWAP_TEST_READY", &ready)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    let mut children = Children(Vec::new());
    for index in 0..16 {
        children.0.push(spawn(
            index,
            if index % 2 == 0 { "one" } else { "two" },
            index % 3 == 0,
        ));
    }
    wait_for(|| fs::read_dir(&ready).unwrap().count() == 16);
    for label in ["one", "two"] {
        assert!(aswap::provider::remove(&store, Provider::Claude, label).is_err());
        assert!(aswap::rename::rename(&store, Provider::Claude, label, "renamed").is_err());
        aswap::provider::switch(&store, Provider::Claude, label, false).unwrap();
    }
    // Force only this session to switch; the best account already has many users.
    children.0.push(spawn(99, "one", true));
    let switched = ready.join("00000000-0000-4000-8000-000000000099-synthetic-two");
    wait_for(|| switched.exists());
    for child in &mut children.0 {
        assert!(child.try_wait().unwrap().is_none());
    }
    // A killed supervisor's provider exits automatically; dead leases cannot block mutations.
    children.0[0].kill().unwrap();
    children.0[0].wait().unwrap();
    fs::write(ready.join("release"), b"synthetic barrier").unwrap();
    for child in children.0.iter_mut().skip(1) {
        wait_for(|| child.try_wait().unwrap().is_some());
        assert!(child.wait().unwrap().success());
    }
    // Any lingering killed child gets a parent-death signal; allow the kernel to reap it.
    wait_for(|| aswap::rename::rename(&store, Provider::Claude, "one", "renamed").is_ok());
    aswap::provider::remove(&store, Provider::Claude, "renamed").unwrap();
    aswap::provider::remove(&store, Provider::Claude, "two").unwrap();
    for label in ["one", "two"] {
        let dir = store.root.join("live/claude").join(label);
        assert_eq!(fs::read_dir(dir).unwrap().count(), 0);
    }
}
