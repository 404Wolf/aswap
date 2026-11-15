//! Process-level recovery tests with an acknowledged rejection barrier and a
//! clock shared only by the test supervisor processes. The production build
//! continues to use crate::now directly.
use super::{Store, run, store, usage};
use crate::provider::Provider;
use std::{
    cell::RefCell,
    collections::BTreeSet,
    fs,
    io::{BufRead, Read},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex, mpsc},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

thread_local! {
    static CLOCK: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

pub(super) fn now() -> u64 {
    CLOCK.with(|clock| match clock.borrow().as_ref() {
        Some(path) => fs::read_to_string(path)
            .expect("test clock is readable")
            .parse()
            .expect("test clock is an epoch second"),
        None => crate::now(),
    })
}

enum Event {
    Line(usize, String),
    Closed(usize),
}

struct Worker {
    child: Child,
    trace: Arc<Mutex<String>>,
    reader: Option<JoinHandle<()>>,
}
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn barrier(
    events: &mpsc::Receiver<Event>,
    workers: &[Worker],
    phase: &str,
    mut ready: impl FnMut(Event) -> bool,
) {
    // This is a deadlock backstop, never the clock driving quota recovery.
    let end = Instant::now() + Duration::from_secs(120);
    loop {
        if let Ok(event) = events.recv_timeout(Duration::from_millis(100))
            && ready(event)
        {
            return;
        }
        assert!(
            Instant::now() < end,
            "{phase} timed out; supervisor traces:\n{}",
            workers
                .iter()
                .map(|worker| worker.trace.lock().unwrap().clone())
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
}

fn set_clock(path: &Path, at: u64) {
    store::atomic(path, at.to_string().as_bytes()).unwrap();
}

fn scenario(provider: Provider, recovered: &str, delayed: bool) {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("data/aswap")).unwrap();
    let base = crate::now();
    let reset = base + 4;
    let clock = temp.path().join("clock");
    set_clock(&clock, base);
    let log = temp.path().join("launches");
    fs::create_dir(&log).unwrap();
    fs::create_dir(temp.path().join("claude")).unwrap();
    fs::create_dir(temp.path().join("codex")).unwrap();
    fs::create_dir(temp.path().join("bin")).unwrap();
    let python = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|path| path.join("python3"))
        .find(|path| path.is_file())
        .unwrap();
    let fake = temp.path().join("bin").join(provider.to_string());
    fs::write(
        &fake,
        format!(
            "#!{}\n{}",
            python.display(),
            include_str!("../../tests/fixtures/wake_provider.py")
        ),
    )
    .unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
    let mut state = store.load().unwrap();
    for (label, used) in [("current", 20.0), ("other", 100.0)] {
        let profile = store.profile(provider, label).unwrap();
        let auth = if provider == Provider::Claude {
            serde_json::json!({"claudeAiOauth":{"accessToken":format!("synthetic-{label}")}})
        } else {
            serde_json::json!({"tokens":{"access_token":format!("synthetic-{label}")}})
        };
        store::json_write(&profile.join(provider.auth_file()), &auth).unwrap();
        store::json_write(
            &profile.join("usage-cache.json"),
            &usage::Cached {
                fetched_at: base,
                // No real usage request is allowed in this fixture. The
                // coordinator publishes the verified observation itself.
                last_attempt: base + 86_400,
                next_poll_at: base + 86_400,
                usage: usage::Usage {
                    windows: vec![usage::Window {
                        name: "weekly".into(),
                        used_percent: used,
                        resets_at: Some(if label == recovered {
                            reset
                        } else {
                            reset + 60
                        }),
                        duration_minutes: Some(10080),
                    }],
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();
        state.accounts.push(store::Account {
            provider,
            label: label.into(),
        });
    }
    state.active.insert(provider, "current".into());
    store.save(&state).unwrap();

    let count = std::env::var("ASWAP_WAKE_TEST_SESSIONS")
        .map(|value| value.parse::<usize>().unwrap())
        .unwrap_or(4);
    assert!((1..=20).contains(&count));
    let (tx, rx) = mpsc::channel();
    let mut workers = Vec::new();
    for index in 0..count {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "runner::wake_process_tests::supervisor_child",
                "--nocapture",
            ])
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("HOME", temp.path())
            .env("XDG_DATA_HOME", temp.path().join("data"))
            .env("XDG_CONFIG_HOME", temp.path().join("config"))
            .env("CLAUDE_CONFIG_DIR", temp.path().join("claude"))
            .env("CODEX_HOME", temp.path().join("codex"))
            .env(provider.bin_var(), &fake)
            .env("ASWAP_TEST_PROVIDER", provider.to_string())
            .env("ASWAP_TEST_INDEX", index.to_string())
            .env("ASWAP_TEST_CLOCK", &clock)
            .env("ASWAP_TEST_LOG", &log)
            .env("ASWAP_TEST_RECOVERED", recovered)
            .env("ASWAP_TEST_EXITED", "yes")
            .env(
                "ASWAP_TEST_MODE",
                if delayed { "delayed" } else { "ordinary" },
            )
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stderr = child.stderr.take().unwrap();
        let trace = Arc::new(Mutex::new(String::new()));
        let recorded = Arc::clone(&trace);
        let tx = tx.clone();
        let reader = thread::spawn(move || {
            for line in std::io::BufReader::new(stderr).lines() {
                let line = line.unwrap();
                recorded
                    .lock()
                    .unwrap()
                    .push_str(&format!("{index}: {line}\n"));
                if tx.send(Event::Line(index, line)).is_err() {
                    return;
                }
            }
            let _ = tx.send(Event::Closed(index));
        });
        workers.push(Worker {
            child,
            trace,
            reader: Some(reader),
        });
    }
    drop(tx);

    if delayed {
        // The red reproduction published recovery while this lock still
        // blocked rejection bookkeeping. Release bookkeeping first instead;
        // recovery remains unpublished until every supervisor acknowledges it.
        let end = Instant::now() + Duration::from_secs(120);
        while fs::read_dir(&log).unwrap().count() != count {
            assert!(
                Instant::now() < end,
                "providers did not reach the controlled limit barrier"
            );
            thread::sleep(Duration::from_millis(10));
        }
        let held = store.lock().unwrap();
        fs::write(temp.path().join("release-limit"), "report limits").unwrap();
        while !fs::read_dir(&log)
            .unwrap()
            .all(|entry| fs::read_to_string(entry.unwrap().path()).unwrap_or_default() == "limited")
        {
            assert!(
                Instant::now() < end,
                "providers did not report their limits"
            );
            thread::sleep(Duration::from_millis(10));
        }
        drop(held);
    }

    let mut waiting = BTreeSet::new();
    barrier(&rx, &workers, "rejection acknowledgement", |event| {
        match event {
            Event::Line(index, line) if line.contains("watching all accounts") => {
                waiting.insert(index);
            }
            Event::Closed(index) => {
                panic!("supervisor {index} exited before recovery was published")
            }
            _ => {}
        }
        waiting.len() == count
    });
    assert!(CLOCK.with(|clock| clock.borrow().is_none()));
    assert_eq!(fs::read_to_string(&clock).unwrap(), base.to_string());
    assert!(
        workers
            .iter_mut()
            .all(|worker| worker.child.try_wait().unwrap().is_none())
    );
    {
        let _lock = store.lock().unwrap();
        let path = store
            .profile(provider, recovered)
            .unwrap()
            .join("usage-cache.json");
        let mut cache: usage::Cached = store::json_read(&path).unwrap();
        cache.error = None;
        cache.fetched_at = reset;
        cache.usage.windows[0].used_percent = 2.0;
        cache.usage.windows[0].resets_at = Some(base + 86_400);
        store::json_write(&path, &cache).unwrap();
    }
    set_clock(&clock, reset + 6); // past every supervisor's 1..5s stagger

    let mut closed = BTreeSet::new();
    barrier(&rx, &workers, "verified recovery", |event| {
        if let Event::Closed(index) = event {
            closed.insert(index);
        }
        closed.len() == count
    });
    for worker in &mut workers {
        let status = worker.child.wait().unwrap();
        let mut stdout = String::new();
        worker
            .child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut stdout)
            .unwrap();
        let trace = worker.trace.lock().unwrap();
        assert!(status.success(), "{trace}\n{stdout}");
        assert!(
            trace.contains(&format!("quota available on {provider}:{recovered}")),
            "{trace}"
        );
    }
    assert_eq!(fs::read_dir(&log).unwrap().count(), 2 * count);
}

#[test]
fn exited_claude_waits_and_many_sessions_resume_on_the_first_reset() {
    for account in ["current", "other"] {
        scenario(Provider::Claude, account, false);
    }
}
#[test]
fn exited_codex_waits_and_many_sessions_resume_on_the_first_reset() {
    for account in ["current", "other"] {
        scenario(Provider::Codex, account, false);
    }
}
#[test]
fn delayed_rejections_wait_for_reset_acknowledgements() {
    scenario(Provider::Claude, "current", true);
}

#[test]
fn supervisor_child() {
    let Some(clock) = std::env::var_os("ASWAP_TEST_CLOCK") else {
        return;
    };
    CLOCK.with(|slot| *slot.borrow_mut() = Some(clock.into()));
    let provider = if std::env::var("ASWAP_TEST_PROVIDER").unwrap() == "claude" {
        Provider::Claude
    } else {
        Provider::Codex
    };
    let index: usize = std::env::var("ASWAP_TEST_INDEX").unwrap().parse().unwrap();
    let id = format!("11111111-2222-4333-8444-{index:012}");
    let args = vec![
        if provider == Provider::Claude {
            "--resume".into()
        } else {
            "resume".into()
        },
        id,
        "--model".into(),
        "synthetic-model".into(),
    ];
    assert_eq!(
        run(
            &Store::from_env().unwrap(),
            provider,
            Some("current"),
            &args,
            true
        )
        .unwrap(),
        0
    );
}
