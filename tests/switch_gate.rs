//! The machine-wide switch gate over real lock and state files in a temp
//! directory: racing waiters with their own file handles, and a waiter
//! process killed while it holds a place in line.
use aswap::switch_gate::{Clock, FileShelf, Shared, Shelf, SwitchGate, SystemClock, Turn, ttl};
use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Barrier, Mutex},
    time::{Duration, Instant},
};

const INTERVAL: Duration = Duration::from_millis(200);
const HELPER_DIR: &str = "ASWAP_SWITCH_GATE_HELPER_DIR";

/// A file shelf that logs, under the gate's lock, who was granted the slot
/// and when, so the log is exact whatever the threads do afterwards.
struct Recording {
    inner: FileShelf,
    who: String,
    log: Arc<Mutex<Vec<(String, u64)>>>,
}
impl Shelf for Recording {
    fn update<R>(&self, change: impl FnOnce(&mut Shared) -> (R, bool)) -> aswap::Result<R> {
        self.inner.update(|shared| {
            let queued = shared.queue.iter().any(|t| t.id == self.who);
            let out = change(shared);
            // Granted: its ticket left the line as the switch was recorded.
            if queued && !shared.queue.iter().any(|t| t.id == self.who) {
                self.log
                    .lock()
                    .unwrap()
                    .push((self.who.clone(), shared.last_switch_at));
            }
            out
        })
    }
}
/// Record a switch an hour ahead: the gate counts it as now, so the next
/// waiter has to queue however slowly this test runs.
fn hold_slot(dir: &Path) {
    FileShelf::new(dir)
        .update(|shared| {
            shared.last_switch_at = SystemClock.now_ms() + 3_600_000;
            ((), true)
        })
        .unwrap();
}
fn read_shared(dir: &Path) -> Shared {
    FileShelf::new(dir)
        .update(|shared| (shared.clone(), false))
        .unwrap()
}

#[test]
fn racing_waiters_switch_one_interval_apart_in_request_order() {
    let dir = tempfile::tempdir().unwrap();
    let log = Arc::new(Mutex::new(Vec::new()));
    let waiters = 6;
    let start = Arc::new(Barrier::new(waiters + 1));
    let mut queued = Vec::new();
    let mut threads = Vec::new();
    hold_slot(dir.path());
    for i in 0..waiters {
        let who = format!("w{i}");
        // Each waiter has its own shelf and so its own lock file handle.
        let mut gate = SwitchGate::new(
            Recording {
                inner: FileShelf::new(dir.path()),
                who: who.clone(),
                log: Arc::clone(&log),
            },
            SystemClock,
            who.clone(),
        );
        // Join the line in a known order, then race.
        assert!(matches!(gate.poll(INTERVAL).unwrap(), Turn::Wait { .. }));
        queued.push(who);
        let start = Arc::clone(&start);
        threads.push(std::thread::spawn(move || {
            start.wait();
            gate.acquire(INTERVAL, |pause| {
                std::thread::sleep(pause);
                Ok(())
            })
            .unwrap();
        }));
    }
    start.wait();
    for thread in threads {
        thread.join().unwrap();
    }
    let log = log.lock().unwrap();
    let order: Vec<_> = log.iter().map(|(who, _)| who.clone()).collect();
    assert_eq!(order, queued);
    let interval = INTERVAL.as_millis() as u64;
    for pair in log.windows(2) {
        assert!(
            pair[1].1 - pair[0].1 >= interval,
            "switches too close: {:?}",
            *log
        );
    }
    assert!(read_shared(dir.path()).queue.is_empty());
}

#[test]
fn waiters_that_stop_waiting_leave_the_line() {
    let dir = tempfile::tempdir().unwrap();
    hold_slot(dir.path());
    let mut gone = SwitchGate::new(FileShelf::new(dir.path()), SystemClock, "gone".into());
    assert!(matches!(
        gone.poll(INTERVAL).unwrap(),
        Turn::Wait { position: 1, .. }
    ));
    drop(gone);
    let started = Instant::now();
    let mut next = SwitchGate::new(FileShelf::new(dir.path()), SystemClock, "next".into());
    next.acquire(INTERVAL, |pause| {
        std::thread::sleep(pause);
        Ok(())
    })
    .unwrap();
    assert!(started.elapsed() < INTERVAL * 3);
}

#[test]
fn a_killed_waiter_process_loses_its_place_after_the_ttl() {
    let dir = tempfile::tempdir().unwrap();
    hold_slot(dir.path());
    let mut helper = Command::new(std::env::current_exe().unwrap())
        .args([
            "killed_waiter_helper",
            "--exact",
            "--include-ignored",
            "--quiet",
        ])
        .env(HELPER_DIR, dir.path())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while read_shared(dir.path()).queue.is_empty() {
        assert!(Instant::now() < deadline, "the helper never queued");
        std::thread::sleep(Duration::from_millis(10));
    }
    helper.kill().unwrap();
    helper.wait().unwrap();
    let dead = read_shared(dir.path()).queue[0].clone();
    assert_eq!(dead.id, "helper");

    let mut next = SwitchGate::new(FileShelf::new(dir.path()), SystemClock, "next".into());
    next.acquire(INTERVAL, |pause| {
        std::thread::sleep(pause);
        Ok(())
    })
    .unwrap();
    let granted = read_shared(dir.path()).last_switch_at;
    let ttl = ttl(INTERVAL.as_millis() as u64);
    assert!(
        granted > dead.heartbeat_at + ttl,
        "the dead ticket held its place for its whole TTL"
    );
    assert!(
        granted < dead.heartbeat_at + ttl + 1_000,
        "the queue moved on promptly"
    );
    assert!(read_shared(dir.path()).queue.is_empty());
}

/// Runs only inside `a_killed_waiter_process_loses_its_place_after_the_ttl`:
/// queues, then waits to be killed without leaving the line.
#[test]
#[ignore = "helper process for the killed-waiter test"]
fn killed_waiter_helper() {
    let Some(dir) = std::env::var_os(HELPER_DIR).map(PathBuf::from) else {
        return;
    };
    let mut gate = SwitchGate::new(FileShelf::new(&dir), SystemClock, "helper".into());
    assert!(matches!(gate.poll(INTERVAL).unwrap(), Turn::Wait { .. }));
    std::thread::sleep(Duration::from_secs(60));
    // Never reached: the parent kills this process.
    let _ = SystemClock.now_ms();
}
