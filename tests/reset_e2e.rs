//! Codex usage-limit resets end to end, against a fake Codex app-server:
//! with every account exhausted, the monitor redeems one reset, once, on the
//! account whose weekly window resets farthest away, and the supervisor
//! waiting on the limit resumes its session there. A monitor that dies
//! mid-request resends the same idempotency key. No real Codex and no real
//! reset: the fake counts "spends" per key.
use aswap::{
    coordination,
    provider::{self, Provider},
    store::{self, AutoReset, Store},
};
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

const ID: &str = "11111111-2222-4333-8444-555555555555";

/// A fake Codex. Its app-server serves usage from `state.json` (label ->
/// "exhausted" | "reset"), offers one reset credit per account, and on
/// `account/rateLimitResetCredit/consume` spends at most once per
/// idempotency key (`spends.jsonl`), logging every request
/// (`consumes.jsonl`). With ASWAP_TEST_HANG=1 the first request for a key is
/// left unanswered (unspent), as if the monitor died before the reply. A
/// session on account `one` hits its limit and exits; a resumed one exits 0.
const FAKE_CODEX: &str = r##"import json, os, pathlib, sys, time
dir=pathlib.Path(os.environ['ASWAP_TEST_DIR'])
home=pathlib.Path(os.environ['CODEX_HOME'])
args=sys.argv[1:]
if args[:1]==['-c']: args=args[2:]
auth=json.loads((home/'auth.json').read_text())['tokens']['access_token']
label=auth.removeprefix('synthetic-')
state=json.loads((dir/'state.json').read_text())
weekly={'one':5,'two':2} if os.environ.get('ASWAP_TEST_FARTHEST')=='one' else {'one':2,'two':5}
def limits(l):
    used=100 if state.get(l,'exhausted')=='exhausted' else 0
    at=int(time.time())
    return {'rateLimits':{
        'primary':{'usedPercent':used,'windowDurationMins':300,'resetsAt':at+3600},
        'secondary':{'usedPercent':used,'windowDurationMins':10080,'resetsAt':at+weekly[l]*86400}},
      'rateLimitResetCredits':{'availableCount':1,'credits':[{'id':'credit-'+l,'status':'available',
        'resetType':'codexRateLimits','expiresAt':at+30*86400}]}}
def lines(name):
    p=dir/name
    return [json.loads(x) for x in p.read_text().splitlines()] if p.exists() else []
def append(name,value):
    with (dir/name).open('a') as f: f.write(json.dumps(value)+'\n')
if 'app-server' in args:
    for line in sys.stdin:
        req=json.loads(line); method=req.get('method'); result=None
        if method=='initialize': result={}
        elif method=='model/list': result={'data':[{'id':'synthetic-model','model':'synthetic-model','isDefault':True}]}
        elif method=='account/rateLimits/read':
            # Optionally, the first read after a reset is slow (3 s).
            if (dir/'slow-post-read').exists() and state.get(label)=='reset' and not (dir/'slow-read-done').exists():
                (dir/'slow-read-done').write_text('')
                time.sleep(3)
            result=limits(label)
        elif method=='account/rateLimitResetCredit/consume':
            key=req['params']['idempotencyKey']
            seen=[c for c in lines('consumes.jsonl') if c['key']==key]
            append('consumes.jsonl',{'key':key,'label':label,'credit':req['params']['creditId']})
            if os.environ.get('ASWAP_TEST_HANG')=='1' and not seen:
                time.sleep(120)
            spent_before_hang = os.environ.get('ASWAP_TEST_HANG')=='after-spend' and not seen
            if any(s['key']==key for s in lines('spends.jsonl')):
                result={'outcome':'alreadyRedeemed'}
            else:
                append('spends.jsonl',{'key':key,'label':label})
                state[label]='reset'
                (dir/'state.json').write_text(json.dumps(state))
                result={'outcome':'reset'}
                if spent_before_hang:
                    # Spent, then the reply is lost.
                    time.sleep(120)
        else: continue
        print(json.dumps({'id':req['id'],'result':result}),flush=True)
    sys.exit(0)
append('launches.jsonl',{'label':label,'args':args})
transcript=home/'sessions'/'2026'/'01'/'01'/('rollout-synthetic-'+os.environ['ASWAP_TEST_ID']+'.jsonl')
transcript.parent.mkdir(parents=True,exist_ok=True)
if 'resume' in args and (dir/'spends.jsonl').exists():
    sys.exit(0)
transcript.write_text('synthetic conversation\n')
print("■ You've hit your usage limit. Try again later.",flush=True)
time.sleep(.5)
sys.exit(1)
"##;

/// A running supervisor, killed (and reaped) if a test fails before it ends.
struct Supervisor(Child);
impl Drop for Supervisor {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Fixture {
    t: tempfile::TempDir,
    fake: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let t = tempfile::tempdir().unwrap();
        let fake = t.path().join("codex");
        let python = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|p| p.join("python3"))
            .find(|p| p.is_file())
            .unwrap();
        fs::write(&fake, format!("#!{}\n{FAKE_CODEX}", python.display())).unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(
            t.path().join("state.json"),
            r#"{"one":"exhausted","two":"exhausted"}"#,
        )
        .unwrap();
        fs::create_dir_all(t.path().join("project")).unwrap();
        Self { t, fake }
    }
    fn dir(&self) -> &Path {
        self.t.path()
    }
    fn command(&self, child: &str, hang: &str) -> Command {
        let farthest = if self.dir().join("farthest-one").exists() {
            "one"
        } else {
            "two"
        };
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", child, "--nocapture"])
            .current_dir(self.dir().join("project"))
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("HOME", self.dir())
            .env("XDG_DATA_HOME", self.dir().join("data"))
            .env("XDG_CONFIG_HOME", self.dir().join("config"))
            .env("CODEX_HOME", self.dir().join("codex-home"))
            .env("CLAUDE_CONFIG_DIR", self.dir().join("claude"))
            .env("ASWAP_CODEX_BIN", &self.fake)
            .env("ASWAP_TEST_DIR", self.dir())
            .env("ASWAP_TEST_ID", ID)
            .env("ASWAP_RESET_CHILD", child)
            .env("ASWAP_TEST_HANG", hang)
            .env("ASWAP_TEST_FARTHEST", farthest)
            .stdin(Stdio::null());
        command
    }
    fn run(&self, child: &str, hang: &str) {
        let out = self.command(child, hang).output().unwrap();
        assert!(
            out.status.success(),
            "{child}\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    fn lines(&self, name: &str) -> Vec<Value> {
        fs::read_to_string(self.dir().join(name))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }
    /// Starts the supervisor and waits until it waits on the limit.
    fn waiting_supervisor(&self) -> (Supervisor, std::sync::mpsc::Receiver<String>) {
        let mut child = Supervisor(
            self.command("supervisor_child", "0")
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let stderr = child.0.stderr.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(|l| l.ok()) {
                let _ = tx.send(line);
            }
        });
        let end = Instant::now() + Duration::from_secs(60);
        loop {
            let line = rx
                .recv_timeout(end.saturating_duration_since(Instant::now()))
                .expect("the supervisor never waited on the limit");
            if line.contains("watching all accounts") {
                // Let the session's CLI exit, so the supervisor is in its
                // waiting loop (next scheduled check minutes away).
                std::thread::sleep(Duration::from_secs(2));
                return (child, rx);
            }
        }
    }
}
fn is_child(name: &str) -> bool {
    std::env::var("ASWAP_RESET_CHILD").as_deref() == Ok(name)
}

#[test]
fn setup_child() {
    if !is_child("setup_child") {
        return;
    }
    let store = Store::from_env().unwrap();
    coordination::owner(&store).unwrap();
    let home = Provider::Codex.current_home().unwrap();
    fs::create_dir_all(&home).unwrap();
    for label in ["one", "two"] {
        store::json_write(
            &home.join("auth.json"),
            &json!({"tokens":{"access_token":format!("synthetic-{label}")}}),
        )
        .unwrap();
        provider::add(&store, Provider::Codex, label).unwrap();
    }
    let _lock = store.lock().unwrap();
    let mut state = store.load().unwrap();
    state.switch_interval_seconds = 0;
    let codex = state.auto.entry(Provider::Codex).or_default();
    codex.enabled = true;
    codex.auto_reset = AutoReset::On;
    store.save(&state).unwrap();
}

#[test]
fn supervisor_child() {
    if !is_child("supervisor_child") {
        return;
    }
    let store = Store::from_env().unwrap();
    let args: Vec<String> = ["resume", ID, "--model", "synthetic-model"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let code = aswap::runner::run(&store, Provider::Codex, Some("one"), &args, true).unwrap();
    assert_eq!(code, 0);
}

#[test]
fn tick_child() {
    if !is_child("tick_child") {
        return;
    }
    let store = Store::from_env().unwrap();
    let step = aswap::reset::tick(&store, Provider::Codex).unwrap();
    eprintln!("step: {step:?}");
}

/// The supervisor resumes the session on account two, and exits.
fn resumed_on_two(fixture: &Fixture, supervisor: Supervisor) {
    resumed_on(fixture, supervisor, "two");
}
fn resumed_on(fixture: &Fixture, supervisor: Supervisor, label: &str) {
    resumed_within(fixture, supervisor, label, 60);
}
fn resumed_within(fixture: &Fixture, mut supervisor: Supervisor, label: &str, secs: u64) {
    let end = Instant::now() + Duration::from_secs(secs);
    let status = loop {
        if let Some(status) = supervisor.0.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < end, "the supervisor never resumed");
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(status.success());
    let launches = fixture.lines("launches.jsonl");
    let labels: Vec<_> = launches.iter().map(|l| l["label"].clone()).collect();
    assert_eq!(labels, [json!("one"), json!(label)], "{launches:?}");
    let resumed: Vec<String> = serde_json::from_value(launches[1]["args"].clone()).unwrap();
    assert_eq!(
        resumed[..2],
        ["resume".to_owned(), ID.to_owned()],
        "{resumed:?}"
    );
}

#[test]
fn an_exhausted_pool_redeems_one_reset_and_the_waiting_session_resumes() {
    let fixture = Fixture::new();
    fixture.run("setup_child", "0");
    let (supervisor, _) = fixture.waiting_supervisor();
    fixture.run("tick_child", "0");
    // One reset, on the account whose weekly window resets farthest away.
    let spends = fixture.lines("spends.jsonl");
    assert_eq!(spends.len(), 1, "{spends:?}");
    assert_eq!(spends[0]["label"], "two");
    resumed_on_two(&fixture, supervisor);
    // Another tick spends nothing: there is room now, and a cooldown.
    fixture.run("tick_child", "0");
    assert_eq!(fixture.lines("spends.jsonl").len(), 1);
    assert_eq!(fixture.lines("consumes.jsonl").len(), 1);
}

#[test]
fn a_monitor_that_dies_mid_request_resends_the_same_key_and_spends_once() {
    let fixture = Fixture::new();
    fixture.run("setup_child", "0");
    let (supervisor, _) = fixture.waiting_supervisor();
    // The first tick sends its request and dies before any reply.
    let mut tick = fixture.command("tick_child", "1").spawn().unwrap();
    let end = Instant::now() + Duration::from_secs(30);
    while fixture.lines("consumes.jsonl").is_empty() {
        assert!(
            Instant::now() < end,
            "the first tick never sent its request"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    tick.kill().unwrap();
    tick.wait().unwrap();
    assert!(fixture.lines("spends.jsonl").is_empty());
    // The next tick picks the saved request up and sends the same key.
    fixture.run("tick_child", "0");
    let consumes = fixture.lines("consumes.jsonl");
    assert_eq!(consumes.len(), 2, "{consumes:?}");
    assert_eq!(consumes[0]["key"], consumes[1]["key"]);
    assert_eq!(fixture.lines("spends.jsonl").len(), 1);
    resumed_on_two(&fixture, supervisor);
}

#[test]
fn a_reset_redeemed_on_the_waiting_sessions_own_account_releases_its_hold() {
    // The session's own account, rejected by its limit, is the one whose
    // quota comes back farthest away: the redeemed reset releases the
    // rejection at once, instead of after its 15-minute hold.
    let fixture = Fixture::new();
    fs::write(fixture.dir().join("farthest-one"), "").unwrap();
    fixture.run("setup_child", "0");
    let (supervisor, _) = fixture.waiting_supervisor();
    fixture.run("tick_child", "0");
    let spends = fixture.lines("spends.jsonl");
    assert_eq!(spends.len(), 1, "{spends:?}");
    assert_eq!(spends[0]["label"], "one");
    resumed_on(&fixture, supervisor, "one");
}

#[test]
fn retry_child() {
    if !is_child("retry_child") {
        return;
    }
    let store = Store::from_env().unwrap();
    let _reset = aswap::reset::change_lock(&store).unwrap();
    let _lock = store.lock().unwrap();
    aswap::reset::settle_paused(&store, false)
        .unwrap()
        .expect("a paused request");
}

#[test]
fn a_spend_whose_reply_is_lost_is_reconciled_without_a_second_send() {
    let fixture = Fixture::new();
    fixture.run("setup_child", "0");
    let (supervisor, _) = fixture.waiting_supervisor();
    // The first tick's request is spent by Codex, and its reply is lost.
    let mut tick = fixture
        .command("tick_child", "after-spend")
        .spawn()
        .unwrap();
    let end = Instant::now() + Duration::from_secs(30);
    while fixture.lines("spends.jsonl").is_empty() {
        assert!(Instant::now() < end, "the first tick never spent");
        std::thread::sleep(Duration::from_millis(50));
    }
    tick.kill().unwrap();
    tick.wait().unwrap();
    // The next tick finds quota back: it pauses the request, sending nothing.
    fixture.run("tick_child", "0");
    assert_eq!(fixture.lines("consumes.jsonl").len(), 1);
    // `reset retry` reconciles it: usable quota means it worked; no resend.
    fixture.run("retry_child", "0");
    fixture.run("tick_child", "0");
    assert_eq!(fixture.lines("consumes.jsonl").len(), 1, "no second send");
    assert_eq!(fixture.lines("spends.jsonl").len(), 1);
    resumed_on_two(&fixture, supervisor);
    let log: Value = serde_json::from_slice(
        &fs::read(fixture.dir().join("data/aswap/codex-resets.json")).unwrap(),
    )
    .unwrap();
    let outcomes: Vec<_> = log["history"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["outcome"].as_str().unwrap().to_owned())
        .collect();
    assert!(outcomes.contains(&"reconciled".to_owned()), "{outcomes:?}");
}

#[test]
fn a_slow_post_reset_read_still_wakes_the_waiting_session_promptly() {
    // The outcome is journaled before the post-reset read lands; the
    // waiting session must wake on the fresh reading (Recovered), not go
    // back to its long poll on the old, full one.
    let fixture = Fixture::new();
    fs::write(fixture.dir().join("farthest-one"), "").unwrap();
    fs::write(fixture.dir().join("slow-post-read"), "").unwrap();
    fixture.run("setup_child", "0");
    let (supervisor, _) = fixture.waiting_supervisor();
    fixture.run("tick_child", "0");
    assert_eq!(fixture.lines("spends.jsonl").len(), 1);
    resumed_within(&fixture, supervisor, "one", 10);
}

#[test]
fn a_monitor_that_dies_before_the_post_reset_read_resumes_it_next_tick() {
    // The outcome is durable but the tick dies during its slow post-reset
    // read: the next tick must resume that read (and journal Recovered), so
    // the waiting session wakes promptly, not at its next long poll.
    let fixture = Fixture::new();
    fs::write(fixture.dir().join("farthest-one"), "").unwrap();
    fs::write(fixture.dir().join("slow-post-read"), "").unwrap();
    fixture.run("setup_child", "0");
    let (supervisor, _) = fixture.waiting_supervisor();
    let mut tick = fixture.command("tick_child", "0").spawn().unwrap();
    let end = Instant::now() + Duration::from_secs(30);
    while !fixture.dir().join("slow-read-done").exists() {
        assert!(
            Instant::now() < end,
            "the tick never reached its post-reset read"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    tick.kill().unwrap();
    tick.wait().unwrap();
    assert_eq!(fixture.lines("spends.jsonl").len(), 1);
    fixture.run("tick_child", "0");
    assert_eq!(fixture.lines("spends.jsonl").len(), 1, "no second spend");
    resumed_within(&fixture, supervisor, "one", 10);
}
