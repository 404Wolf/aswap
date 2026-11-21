//! The machine-wide switch gate over real lock and state files in a temp
//! directory: racing threads with their own file handles, racing processes,
//! a waiter process killed while it holds a place in line, and a lock held
//! for a moment or for good.
use aswap::switch_gate::{
    Clock, FileShelf, LOCK_WAIT, Shared, Shelf, SwitchGate, SystemClock, Turn, busy_limit, ttl,
};
use fs2::FileExt;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Barrier,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

const INTERVAL: Duration = Duration::from_millis(200);
const HELPER_DIR: &str = "ASWAP_SWITCH_GATE_HELPER_DIR";
const HELPER_WHO: &str = "ASWAP_SWITCH_GATE_HELPER_WHO";
const HELPER_START: &str = "ASWAP_SWITCH_GATE_HELPER_START";

/// A temp directory by its real path: on macOS the temp root is behind a
/// symlink, which the gate (like the store) refuses.
fn temp() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().canonicalize().unwrap();
    (dir, path)
}

/// A file shelf that appends, under the gate's lock, when `who` joined the
/// line and when it was granted, so the log is in lock order whatever the
/// waiters (threads or processes) do afterwards.
struct Recording {
    inner: FileShelf,
    who: String,
    log: PathBuf,
}
impl Shelf for Recording {
    fn update<R>(&self, change: impl FnOnce(&mut Shared) -> (R, bool)) -> aswap::Result<R> {
        self.inner.update(|shared| {
            let queued = shared.queue.iter().any(|t| t.id == self.who);
            let last = shared.last_switch_at;
            let out = change(shared);
            let enqueue = format!("enqueue {}\n", self.who);
            let grant = format!("grant {} {}\n", self.who, shared.last_switch_at);
            let line = match (queued, shared.queue.iter().any(|t| t.id == self.who)) {
                (false, true) => Some(enqueue),
                (true, false) => Some(grant),
                // A straggler that asked after the line emptied goes at once.
                (false, false) if shared.last_switch_at != last => Some(enqueue + &grant),
                _ => None,
            };
            if let Some(line) = line {
                fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&self.log)
                    .unwrap()
                    .write_all(line.as_bytes())
                    .unwrap();
            }
            out
        })
    }
}
fn recording(dir: &Path, who: &str) -> SwitchGate<Recording, SystemClock> {
    SwitchGate::new(
        Recording {
            inner: FileShelf::new(dir),
            who: who.into(),
            log: dir.join("log"),
        },
        SystemClock,
        who.into(),
    )
}
fn sleep(pause: Duration) -> aswap::Result<()> {
    std::thread::sleep(pause);
    Ok(())
}
/// Every waiter joined the line before any was granted, and grants follow
/// the order they joined, at least one interval apart.
fn assert_fifo_and_spaced(dir: &Path, waiters: usize) {
    let log = fs::read_to_string(dir.join("log")).unwrap();
    let (mut joined, mut granted) = (Vec::new(), Vec::new());
    for line in log.lines() {
        match line.split(' ').collect::<Vec<_>>()[..] {
            ["enqueue", who] => joined.push(who.to_owned()),
            ["grant", who, at] => granted.push((who.to_owned(), at.parse::<u64>().unwrap())),
            _ => panic!("bad log line {line}"),
        }
    }
    assert_eq!(joined.len(), waiters, "{log}");
    let order: Vec<_> = granted.iter().map(|(who, _)| who.clone()).collect();
    assert_eq!(order, joined, "{log}");
    for pair in granted.windows(2) {
        assert!(
            pair[1].1 - pair[0].1 >= INTERVAL.as_millis() as u64,
            "grants too close: {log}"
        );
    }
    assert!(read_shared(dir).queue.is_empty());
}
/// Record a switch an hour ahead: the gate takes that for a clock that
/// stepped back and counts it as now, so every waiter has to queue however
/// slowly this test runs.
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
fn racing_threads_switch_one_interval_apart_in_the_order_they_asked() {
    let (_guard, dir) = temp();
    hold_slot(&dir);
    let waiters = 6;
    let start = Arc::new(Barrier::new(waiters));
    let threads: Vec<_> = (0..waiters)
        .map(|i| {
            let (dir, start) = (dir.clone(), Arc::clone(&start));
            std::thread::spawn(move || {
                // Each waiter has its own shelf and so its own lock handle,
                // and all of them ask at once.
                let mut gate = recording(&dir, &format!("t{i}"));
                start.wait();
                gate.acquire(INTERVAL, sleep).unwrap();
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    assert_fifo_and_spaced(&dir, waiters);
}

#[test]
fn racing_processes_switch_one_interval_apart_in_the_order_they_asked() {
    let (_guard, dir) = temp();
    hold_slot(&dir);
    let waiters = 5;
    // Every process asks at the same wall-clock moment, once all have started.
    let start = SystemClock.now_ms() + 1_500;
    let children: Vec<_> = (0..waiters)
        .map(|i| {
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "racing_process_helper",
                    "--exact",
                    "--include-ignored",
                    "--quiet",
                ])
                .env(HELPER_DIR, &dir)
                .env(HELPER_WHO, format!("p{i}"))
                .env(HELPER_START, start.to_string())
                .spawn()
                .unwrap()
        })
        .collect();
    for mut child in children {
        assert!(child.wait().unwrap().success());
    }
    assert_fifo_and_spaced(&dir, waiters);
}

/// Runs only inside `racing_processes_switch_one_interval_apart_in_the_order_they_asked`.
#[test]
#[ignore = "helper process for the racing-processes test"]
fn racing_process_helper() {
    let Some(dir) = std::env::var_os(HELPER_DIR).map(PathBuf::from) else {
        return;
    };
    let who = std::env::var(HELPER_WHO).unwrap();
    let start: u64 = std::env::var(HELPER_START).unwrap().parse().unwrap();
    std::thread::sleep(Duration::from_millis(
        start.saturating_sub(SystemClock.now_ms()),
    ));
    recording(&dir, &who).acquire(INTERVAL, sleep).unwrap();
}

#[test]
fn waiters_that_stop_waiting_leave_the_line() {
    let (_guard, dir) = temp();
    hold_slot(&dir);
    let mut gone = SwitchGate::new(FileShelf::new(&dir), SystemClock, "gone".into());
    assert!(matches!(
        gone.poll(INTERVAL).unwrap(),
        Turn::Wait { position: 1, .. }
    ));
    drop(gone);
    let started = Instant::now();
    let mut next = SwitchGate::new(FileShelf::new(&dir), SystemClock, "next".into());
    next.acquire(INTERVAL, sleep).unwrap();
    assert!(started.elapsed() < INTERVAL * 3);
}

#[test]
fn a_killed_waiter_process_loses_its_place_after_the_ttl() {
    let (_guard, dir) = temp();
    hold_slot(&dir);
    let mut helper = Command::new(std::env::current_exe().unwrap())
        .args([
            "killed_waiter_helper",
            "--exact",
            "--include-ignored",
            "--quiet",
        ])
        .env(HELPER_DIR, &dir)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while read_shared(&dir).queue.is_empty() {
        assert!(Instant::now() < deadline, "the helper never queued");
        std::thread::sleep(Duration::from_millis(10));
    }
    helper.kill().unwrap();
    helper.wait().unwrap();
    let dead = read_shared(&dir).queue[0].clone();
    assert_eq!(dead.id, "helper");

    let mut next = SwitchGate::new(FileShelf::new(&dir), SystemClock, "next".into());
    next.acquire(INTERVAL, sleep).unwrap();
    let granted = read_shared(&dir).last_switch_at;
    let ttl = ttl(INTERVAL.as_millis() as u64);
    assert!(
        granted > dead.heartbeat_at + ttl,
        "the dead ticket held its place for its whole TTL"
    );
    assert!(
        granted < dead.heartbeat_at + ttl + 1_000,
        "the queue moved on promptly"
    );
    assert!(read_shared(&dir).queue.is_empty());
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

/// Holds the gate lock from another open file description, as a holder
/// stalled inside its read-modify-write would.
fn stuck_holder(dir: &Path) -> fs::File {
    let stuck = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("switch-gate.lock"))
        .unwrap();
    stuck.lock_exclusive().unwrap();
    stuck
}

#[test]
fn a_lock_stuck_for_a_moment_is_waited_out_and_the_switch_still_gated() {
    let (_guard, dir) = temp();
    // A switch was just granted; then its holder stalls for a second.
    FileShelf::new(&dir)
        .update(|shared| {
            shared.last_switch_at = SystemClock.now_ms();
            ((), true)
        })
        .unwrap();
    let started = Instant::now();
    let asked = SystemClock.now_ms();
    let stuck = stuck_holder(&dir);
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(1));
        drop(stuck);
    });
    let mut gate = SwitchGate::new(FileShelf::new(&dir), SystemClock, "a".into());
    gate.acquire(INTERVAL, sleep).unwrap();
    release.join().unwrap();
    assert!(started.elapsed() >= Duration::from_secs(1));
    assert!(started.elapsed() < busy_limit(INTERVAL));
    let granted = read_shared(&dir).last_switch_at;
    assert!(granted >= asked + 1_000, "granted only after the stall");
}

/// Wall time that moves on only when a waiter sleeps, ten seconds a time.
struct Jumping(Arc<AtomicU64>);
impl Clock for Jumping {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

#[test]
fn a_lock_stuck_for_good_lets_the_switch_through_after_the_limit() {
    let (_guard, dir) = temp();
    let stuck = stuck_holder(&dir);
    let at = Arc::new(AtomicU64::new(SystemClock.now_ms()));
    let start = at.load(Ordering::SeqCst);
    let mut gate = SwitchGate::new(FileShelf::new(&dir), Jumping(Arc::clone(&at)), "a".into());
    let mut waits = 0;
    gate.acquire(INTERVAL, |pause| {
        assert_eq!(pause, LOCK_WAIT);
        waits += 1;
        at.fetch_add(10_000, Ordering::SeqCst);
        Ok(())
    })
    .unwrap();
    assert_eq!(waits, 3, "retried until 30 s had passed");
    assert!(at.load(Ordering::SeqCst) - start >= busy_limit(INTERVAL).as_millis() as u64);
    drop(stuck);
    assert!(read_shared(&dir).queue.is_empty(), "nothing was written");
}

#[test]
fn a_symlink_in_place_of_the_state_is_replaced_not_followed() {
    let (_guard, dir) = temp();
    let target = dir.join("elsewhere");
    fs::write(&target, b"untouched").unwrap();
    std::os::unix::fs::symlink(&target, dir.join("switch-gate.json")).unwrap();
    let mut gate = SwitchGate::new(FileShelf::new(&dir), SystemClock, "a".into());
    assert_eq!(gate.poll(INTERVAL).unwrap(), Turn::Go);
    assert_eq!(fs::read(&target).unwrap(), b"untouched");
    let state = fs::symlink_metadata(dir.join("switch-gate.json")).unwrap();
    assert!(state.file_type().is_file());
    assert!(read_shared(&dir).last_switch_at > 0);
}
