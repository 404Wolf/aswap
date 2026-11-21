//! A machine-wide cap on supervisor restarts: at most one account switch
//! every `interval` across every aswap supervisor of this user (all
//! providers), granted in the order the supervisors asked. After a common
//! limit or reset, sessions move one at a time instead of as a herd.
//!
//! The shared state is a small JSON file next to `state.json`, changed only
//! under an exclusive advisory lock on a sibling lock file (`flock` through
//! `fs2`, on Linux and macOS alike). The lock covers one read-modify-write;
//! waiters sleep with it released. Liveness needs no process table: a waiter
//! refreshes its ticket's heartbeat while it waits, a ticket whose heartbeat
//! is older than [`ttl`] is dropped, so a killed waiter cannot wedge the
//! queue for longer than that (18 s at the default 4 s: a SIGSTOPped or
//! killed head holds everyone behind it that long), and a waiter that stops
//! waiting removes its ticket.
//!
//! Every lock wait and write is bounded: the lock is tried for at most
//! [`LOCK_WAIT`] and the state is written once, without the store's retries.
//! Either failing lets the switch through (logged), since a stranded session
//! is worse than one early switch.
//!
//! The gate is per account store (`$XDG_DATA_HOME/aswap`): it spaces *grants*;
//! the account commit and launch that follow each grant take their own time.
//!
//! [`decide`] is the whole policy, as a pure function of the shared state and
//! the time; [`SwitchGate`] applies it through a [`Shelf`] (lock and state
//! file) and a [`Clock`], and sleeps through a caller-supplied wait.
use crate::{Result, Safe};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    hash::BuildHasher,
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// The default `switch_interval_seconds`; 0 turns the gate off.
pub const DEFAULT_INTERVAL_SECONDS: u64 = 4;
/// How long past two intervals a silent waiter keeps its place, at most
/// (short intervals get four intervals of slack).
const SLACK_MS: u64 = 10_000;
/// The longest wait for the gate lock, which others hold only for one
/// read-modify-write; past it the gate counts as unusable.
pub const LOCK_WAIT: Duration = Duration::from_millis(1_500);
/// Bounds on how often a waiter behind the head re-reads the queue.
const RECHECK_MIN_MS: u64 = 50;
const RECHECK_MAX_MS: u64 = 1_000;

/// One supervisor waiting to switch. Times are milliseconds since the epoch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ticket {
    pub id: String,
    pub enqueued_at: u64,
    pub heartbeat_at: u64,
}
/// The contents of `switch-gate.json`. The queue is in request order.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shared {
    #[serde(default)]
    pub last_switch_at: u64,
    #[serde(default)]
    pub queue: Vec<Ticket>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Turn {
    /// Switch now; the slot is recorded and the ticket is gone.
    Go,
    /// `position` is 1-based; `eta` estimates the wait for the slot, and
    /// `recheck` is how long to sleep before asking again.
    Wait {
        position: usize,
        eta: Duration,
        recheck: Duration,
    },
}

/// A ticket not refreshed for this long belongs to a waiter that died. The
/// head refreshes its ticket on every poll and sleeps at most one interval;
/// the rest poll at least every second and refresh every quarter TTL.
pub fn ttl(interval_ms: u64) -> u64 {
    let slack = interval_ms.saturating_mul(4).min(SLACK_MS);
    interval_ms.saturating_mul(2).saturating_add(slack)
}
/// Heartbeats are written this often, not on every poll.
fn heartbeat_every(interval_ms: u64) -> u64 {
    ttl(interval_ms) / 4
}

/// Ask for the slot as `id` at `now`. Registers `id` at the tail on its first
/// request, refreshes its heartbeat, drops dead tickets, and grants the slot
/// to the head of the queue once `interval_ms` has passed since the last
/// switch. Returns the turn and whether `shared` changed and must be saved.
///
/// `now` must be read under the gate lock. The recorded switch never moves
/// back: a timestamp up to one TTL ahead of `now` (a reader a moment behind
/// the last writer) stands. One further ahead means the clock stepped back,
/// and counts as `now`, so a backward step delays the next switch by one
/// interval, never more.
pub fn decide(shared: &mut Shared, id: &str, now: u64, interval_ms: u64) -> (Turn, bool) {
    let before = shared.clone();
    if interval_ms == 0 {
        shared.queue.retain(|t| t.id != id);
        let changed = *shared != before;
        return (Turn::Go, changed);
    }
    let ttl = ttl(interval_ms);
    let stepped_back = |at: &mut u64| {
        if *at > now.saturating_add(ttl) {
            *at = now;
        }
    };
    stepped_back(&mut shared.last_switch_at);
    shared.queue.retain_mut(|t| {
        stepped_back(&mut t.enqueued_at);
        stepped_back(&mut t.heartbeat_at);
        t.id == id || now.saturating_sub(t.heartbeat_at) <= ttl
    });
    let position = match shared.queue.iter().position(|t| t.id == id) {
        Some(position) => {
            let ticket = &mut shared.queue[position];
            // The head sleeps up to an interval between polls: refresh it on
            // every one, so a short TTL never drops a live head.
            if position == 0
                || now.saturating_sub(ticket.heartbeat_at) >= heartbeat_every(interval_ms)
            {
                ticket.heartbeat_at = ticket.heartbeat_at.max(now);
            }
            position
        }
        None => {
            shared.queue.push(Ticket {
                id: id.to_owned(),
                enqueued_at: now,
                heartbeat_at: now,
            });
            shared.queue.len() - 1
        }
    };
    let free_at = if shared.last_switch_at == 0 {
        0
    } else {
        shared.last_switch_at.saturating_add(interval_ms)
    };
    let turn = if position == 0 && now >= free_at {
        shared.queue.remove(0);
        shared.last_switch_at = shared.last_switch_at.max(now);
        Turn::Go
    } else {
        let until_free = free_at.saturating_sub(now);
        // The head sleeps until its slot; the rest re-read the queue.
        let recheck = if position == 0 {
            until_free
        } else {
            (interval_ms / 4).clamp(RECHECK_MIN_MS, RECHECK_MAX_MS)
        };
        Turn::Wait {
            position: position + 1,
            eta: Duration::from_millis(
                until_free.saturating_add(interval_ms.saturating_mul(position as u64)),
            ),
            recheck: Duration::from_millis(recheck),
        }
    };
    let changed = *shared != before;
    (turn, changed)
}
/// Withdraw `id`; true when `shared` changed.
pub fn leave(shared: &mut Shared, id: &str) -> bool {
    let before = shared.queue.len();
    shared.queue.retain(|t| t.id != id);
    shared.queue.len() != before
}

/// Wall-clock time shared by every process on the machine.
pub trait Clock {
    fn now_ms(&self) -> u64;
}
pub struct SystemClock;
impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
    }
}

/// Exclusive read-modify-write of the shared state. `change` returns its
/// result and whether the state must be saved.
pub trait Shelf {
    fn update<R>(&self, change: impl FnOnce(&mut Shared) -> (R, bool)) -> Result<R>;
}
/// `switch-gate.json` guarded by `switch-gate.lock` in the store directory.
pub struct FileShelf {
    lock: PathBuf,
    state: PathBuf,
}
impl FileShelf {
    pub fn new(dir: &Path) -> Self {
        Self {
            lock: dir.join("switch-gate.lock"),
            state: dir.join("switch-gate.json"),
        }
    }
}
impl FileShelf {
    /// Take the lock within [`LOCK_WAIT`]; a holder only ever keeps it for one
    /// read-modify-write, so a longer wait means it is stuck (stopped, or on
    /// a stalled disk).
    fn lock(&self) -> Result<fs::File> {
        crate::store::no_links(&self.lock)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&self.lock)
            .safe("cannot open the switch gate lock")?;
        let deadline = Instant::now() + LOCK_WAIT;
        let contended = fs2::lock_contended_error().raw_os_error();
        loop {
            match lock.try_lock_exclusive() {
                Ok(()) => return Ok(lock),
                Err(err) if err.raw_os_error() == contended && Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(err) if err.raw_os_error() == contended => {
                    return Err(crate::Error("the switch gate lock is held too long"));
                }
                Err(_) => return Err(crate::Error("cannot lock the switch gate")),
            }
        }
    }
    /// One attempt, no retries: replace the state with a fresh private file.
    /// No fsync either: the queue only coordinates running supervisors, and
    /// after a crash an empty queue is correct.
    fn write(&self, shared: &Shared) -> Result<()> {
        let dir = self
            .state
            .parent()
            .ok_or(crate::Error("missing store directory"))?;
        let bytes = serde_json::to_vec(shared).safe("cannot encode the switch gate")?;
        let mut temp = tempfile::Builder::new()
            .prefix(".switch-gate")
            .tempfile_in(dir)
            .safe("cannot write the switch gate")?;
        temp.write_all(&bytes)
            .safe("cannot write the switch gate")?;
        temp.persist(&self.state)
            .safe("cannot write the switch gate")?;
        Ok(())
    }
}
impl Shelf for FileShelf {
    fn update<R>(&self, change: impl FnOnce(&mut Shared) -> (R, bool)) -> Result<R> {
        let lock = self.lock()?;
        // Unreadable or corrupt state only forgets the queue: at worst one
        // extra switch goes through early.
        let mut shared = match fs::symlink_metadata(&self.state) {
            Ok(_) => crate::store::read_private(&self.state)
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                .unwrap_or_else(|| {
                    eprintln!(
                        "aswap: the switch gate state was unreadable; starting an empty queue"
                    );
                    Shared::default()
                }),
            Err(_) => Shared::default(),
        };
        let (result, changed) = change(&mut shared);
        if changed {
            self.write(&shared)?;
        }
        drop(lock);
        Ok(result)
    }
}

/// One supervisor's place in the machine-wide switch queue.
pub struct SwitchGate<S: Shelf = FileShelf, C: Clock = SystemClock> {
    shelf: S,
    clock: C,
    id: String,
    queued: bool,
}
impl SwitchGate {
    /// The gate shared by every supervisor using this store. No I/O yet.
    pub fn open(store: &crate::store::Store) -> Self {
        let random = std::collections::hash_map::RandomState::new().hash_one(SystemClock.now_ms());
        Self::new(
            FileShelf::new(&store.root),
            SystemClock,
            format!("{}-{random:016x}", std::process::id()),
        )
    }
}
impl<S: Shelf, C: Clock> SwitchGate<S, C> {
    pub fn new(shelf: S, clock: C, id: String) -> Self {
        Self {
            shelf,
            clock,
            id,
            queued: false,
        }
    }
    /// Ask once without waiting; a `Wait` keeps this gate's place in line.
    pub fn poll(&mut self, interval: Duration) -> Result<Turn> {
        let interval_ms = u64::try_from(interval.as_millis()).unwrap_or(u64::MAX);
        let (id, clock) = (&self.id, &self.clock);
        // Read the clock under the lock: a time read before a wait for it
        // would be stale by that wait.
        let turn = self
            .shelf
            .update(|shared| decide(shared, id, clock.now_ms(), interval_ms))?;
        self.queued = turn != Turn::Go;
        Ok(turn)
    }
    /// Wait for the slot, calling `wait` to sleep between checks (it may
    /// pump a terminal; its error ends the wait and gives up the place).
    /// Logs one line when the switch is delayed. A gate that cannot be read
    /// or written lets the switch through rather than strand a session.
    pub fn acquire(
        &mut self,
        interval: Duration,
        mut wait: impl FnMut(Duration) -> Result<()>,
    ) -> Result<()> {
        if interval.is_zero() {
            self.leave();
            return Ok(());
        }
        let mut reported: Option<usize> = None;
        loop {
            match self.poll(interval) {
                Ok(Turn::Go) => return Ok(()),
                Ok(Turn::Wait {
                    position,
                    eta,
                    recheck,
                }) => {
                    // Once, and again if this waiter lost its place (a stall
                    // past the TTL puts it back at the tail).
                    if reported.is_none_or(|before| position > before) {
                        eprintln!(
                            "aswap: waiting for the global switch slot: position {position}, ~{}s",
                            eta.as_millis().div_ceil(1000)
                        );
                    }
                    reported = Some(position);
                    if let Err(err) = wait(recheck) {
                        self.leave();
                        return Err(err);
                    }
                }
                Err(err) => {
                    eprintln!(
                        "aswap: WARNING: the global switch gate is unusable ({err}); switching now without waiting for the slot"
                    );
                    // Try not to hold up the queue; a ticket left behind
                    // expires with its TTL.
                    self.leave();
                    self.queued = false;
                    return Ok(());
                }
            }
        }
    }
    /// Give up this gate's place in line, if it has one.
    pub fn leave(&mut self) {
        if self.queued {
            let id = &self.id;
            let _ = self.shelf.update(|shared| ((), leave(shared, id)));
            self.queued = false;
        }
    }
}
impl<S: Shelf, C: Clock> Drop for SwitchGate<S, C> {
    fn drop(&mut self) {
        self.leave();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    const I: u64 = 4_000;

    fn wait(turn: Turn) -> (usize, u64) {
        match turn {
            Turn::Wait { position, eta, .. } => (position, eta.as_millis() as u64),
            Turn::Go => panic!("expected to wait"),
        }
    }

    #[test]
    fn the_first_switch_goes_at_once_and_the_next_waits_one_interval() {
        let mut s = Shared::default();
        assert_eq!(decide(&mut s, "a", 100_000, I), (Turn::Go, true));
        assert_eq!(s.last_switch_at, 100_000);
        assert!(s.queue.is_empty());
        assert_eq!(wait(decide(&mut s, "b", 101_000, I).0), (1, 3_000));
        assert_eq!(wait(decide(&mut s, "b", 103_999, I).0), (1, 1));
        assert_eq!(decide(&mut s, "b", 104_000, I).0, Turn::Go);
        assert_eq!(s.last_switch_at, 104_000);
    }

    #[test]
    fn switches_are_granted_in_request_order() {
        let mut s = Shared {
            last_switch_at: 100_000,
            queue: Vec::new(),
        };
        for (i, id) in ["c", "a", "b"].iter().enumerate() {
            assert_eq!(wait(decide(&mut s, id, 100_001 + i as u64, I).0).0, i + 1);
        }
        // Later polls never reorder: "a" and "b" ask first but wait for "c".
        let mut at = 104_000;
        let mut order = Vec::new();
        while order.len() < 3 {
            for id in ["b", "a", "c"] {
                if !order.contains(&id) && decide(&mut s, id, at, I).0 == Turn::Go {
                    order.push(id);
                }
            }
            at += 500;
        }
        assert_eq!(order, ["c", "a", "b"]);
    }

    #[test]
    fn waiting_estimates_count_everyone_ahead() {
        let mut s = Shared {
            last_switch_at: 100_000,
            queue: Vec::new(),
        };
        decide(&mut s, "a", 101_000, I);
        decide(&mut s, "b", 101_000, I);
        let (position, eta) = wait(decide(&mut s, "c", 101_000, I).0);
        assert_eq!((position, eta), (3, 3_000 + 2 * I));
        let Turn::Wait { recheck, .. } = decide(&mut s, "a", 101_500, I).0 else {
            panic!()
        };
        assert_eq!(
            recheck,
            Duration::from_millis(2_500),
            "the head sleeps to its slot"
        );
        let Turn::Wait { recheck, .. } = decide(&mut s, "c", 101_500, I).0 else {
            panic!()
        };
        assert_eq!(recheck, Duration::from_millis(1_000));
    }

    #[test]
    fn a_dead_waiter_is_dropped_after_its_ttl() {
        let mut s = Shared {
            last_switch_at: 100_000,
            queue: Vec::new(),
        };
        decide(&mut s, "dead", 100_500, I);
        decide(&mut s, "live", 100_500, I);
        // "dead" never asks again; "live" keeps polling behind it.
        let mut at = 100_500;
        while at + 500 - 100_500 <= ttl(I) {
            at += 500;
            assert_ne!(decide(&mut s, "live", at, I).0, Turn::Go, "at {at}");
        }
        assert_eq!(decide(&mut s, "live", at + 500, I).0, Turn::Go);
        assert!(s.queue.is_empty(), "the dead ticket is gone too");
    }

    #[test]
    fn heartbeats_are_written_only_when_due() {
        let mut s = Shared {
            last_switch_at: 100_000,
            queue: Vec::new(),
        };
        decide(&mut s, "a", 100_000, I);
        decide(&mut s, "b", 100_000, I);
        let (_, changed) = decide(&mut s, "b", 100_100, I);
        assert!(!changed, "an early poll leaves the file alone");
        let at = 100_000 + heartbeat_every(I);
        let (_, changed) = decide(&mut s, "b", at, I);
        assert!(changed);
        assert_eq!(s.queue[1].heartbeat_at, at);
    }

    #[test]
    fn a_switch_recorded_in_the_future_counts_as_now() {
        let mut s = Shared {
            last_switch_at: 10_000_000,
            queue: vec![Ticket {
                id: "old".into(),
                enqueued_at: 20_000_000,
                heartbeat_at: 20_000_000,
            }],
        };
        let (turn, changed) = decide(&mut s, "a", 100_000, I);
        assert!(changed);
        assert_eq!(s.last_switch_at, 100_000);
        assert_eq!(s.queue[0].heartbeat_at, 100_000);
        assert_eq!(wait(turn), (2, I + I));
        // The future ticket no longer lives forever.
        let at = 100_000 + ttl(I) + 1;
        decide(&mut s, "a", at - 1, I);
        assert_eq!(decide(&mut s, "a", at, I).0, Turn::Go);
    }

    #[test]
    fn interval_zero_switches_immediately_and_records_nothing() {
        let mut s = Shared {
            last_switch_at: 100_000,
            queue: Vec::new(),
        };
        for at in [100_000, 100_001, 100_002] {
            assert_eq!(decide(&mut s, "a", at, 0), (Turn::Go, false));
        }
        decide(&mut s, "b", 100_003, I);
        assert_eq!(decide(&mut s, "b", 100_004, 0), (Turn::Go, true));
        assert!(s.queue.is_empty());
    }

    #[test]
    fn leaving_frees_the_head_for_the_next_waiter() {
        let mut s = Shared {
            last_switch_at: 100_000,
            queue: Vec::new(),
        };
        decide(&mut s, "a", 100_100, I);
        decide(&mut s, "b", 100_100, I);
        assert!(leave(&mut s, "a"));
        assert!(!leave(&mut s, "a"));
        assert_eq!(decide(&mut s, "b", 104_000, I).0, Turn::Go);
    }

    #[test]
    fn a_reader_behind_the_last_switch_never_moves_it_back() {
        // Reviewer repro: B is granted at 10 s; A's time reads 9 s.
        let mut s = Shared::default();
        assert_eq!(decide(&mut s, "b", 10_000, I).0, Turn::Go);
        let _ = decide(&mut s, "a", 9_000, I);
        assert_eq!(s.last_switch_at, 10_000, "B's switch stands");
        // At 13 s, 3 s after B's switch, A still waits; at 14 s it goes.
        assert_eq!(wait(decide(&mut s, "a", 13_000, I).0), (1, 1_000));
        assert_eq!(decide(&mut s, "a", 14_000, I).0, Turn::Go);
        // A grant at a lagging time keeps the later recorded switch.
        let mut s = Shared {
            last_switch_at: 20_000,
            queue: Vec::new(),
        };
        decide(&mut s, "c", 24_000, I);
        s.last_switch_at = 25_000;
        assert_eq!(wait(decide(&mut s, "c", 24_500, I).0).0, 1);
        assert_eq!(decide(&mut s, "c", 29_000, I).0, Turn::Go);
        assert_eq!(s.last_switch_at, 29_000);
    }

    #[test]
    fn the_head_refreshes_its_ticket_on_every_poll() {
        let mut s = Shared {
            last_switch_at: 100_000,
            queue: Vec::new(),
        };
        decide(&mut s, "a", 100_100, I);
        let (_, changed) = decide(&mut s, "a", 100_200, I);
        assert!(changed);
        assert_eq!(s.queue[0].heartbeat_at, 100_200);
    }

    struct Memory(Rc<RefCell<Shared>>, Rc<Cell<bool>>);
    impl Shelf for Memory {
        fn update<R>(&self, change: impl FnOnce(&mut Shared) -> (R, bool)) -> Result<R> {
            if self.1.get() {
                return Err(crate::Error("synthetic gate failure"));
            }
            Ok(change(&mut self.0.borrow_mut()).0)
        }
    }
    struct Fake(Rc<Cell<u64>>);
    impl Clock for Fake {
        fn now_ms(&self) -> u64 {
            self.0.get()
        }
    }

    #[test]
    fn acquire_sleeps_through_the_port_and_drop_gives_up_the_place() {
        let shared = Rc::new(RefCell::new(Shared {
            last_switch_at: 100_000,
            queue: Vec::new(),
        }));
        let broken = Rc::new(Cell::new(false));
        let clock = Rc::new(Cell::new(101_000));
        let mut gate = SwitchGate::new(
            Memory(shared.clone(), broken.clone()),
            Fake(clock.clone()),
            "a".into(),
        );
        let mut slept = Vec::new();
        gate.acquire(Duration::from_millis(I), |d| {
            slept.push(d);
            clock.set(clock.get() + d.as_millis() as u64);
            Ok(())
        })
        .unwrap();
        assert_eq!(slept, [Duration::from_millis(3_000)]);
        assert_eq!(shared.borrow().last_switch_at, 104_000);

        let mut waiting = SwitchGate::new(
            Memory(shared.clone(), broken.clone()),
            Fake(clock.clone()),
            "b".into(),
        );
        assert!(matches!(
            waiting.poll(Duration::from_millis(I)).unwrap(),
            Turn::Wait { .. }
        ));
        assert_eq!(shared.borrow().queue.len(), 1);
        drop(waiting);
        assert!(shared.borrow().queue.is_empty());

        // An interrupted wait leaves the queue; a broken gate lets it through.
        let mut gate = SwitchGate::new(
            Memory(shared.clone(), broken.clone()),
            Fake(clock.clone()),
            "c".into(),
        );
        let err = gate.acquire(Duration::from_millis(I), |_| {
            Err(crate::Error("interrupted"))
        });
        assert!(err.is_err());
        assert!(shared.borrow().queue.is_empty());
        broken.set(true);
        gate.acquire(Duration::from_millis(I), |_| panic!("no wait"))
            .unwrap();
    }

    /// A shelf whose lock takes `delay` to get, on a shared fake clock.
    struct Slow(Rc<RefCell<Shared>>, Rc<Cell<u64>>, u64);
    impl Shelf for Slow {
        fn update<R>(&self, change: impl FnOnce(&mut Shared) -> (R, bool)) -> Result<R> {
            self.1.set(self.1.get() + self.2);
            Ok(change(&mut self.0.borrow_mut()).0)
        }
    }

    #[test]
    fn a_poll_reads_the_clock_after_its_wait_for_the_lock() {
        // Reviewer repro: a poller that stalls 30 s on its way to the lock
        // (more than the TTL) must decide with the time it got the lock, or
        // it rewinds the last switch and drops live waiters as dead.
        let shared = Rc::new(RefCell::new(Shared::default()));
        let clock = Rc::new(Cell::new(100_000));
        let gate = |id: &str, delay| {
            SwitchGate::new(
                Slow(shared.clone(), clock.clone(), delay),
                Fake(clock.clone()),
                id.to_owned(),
            )
        };
        let interval = Duration::from_millis(I);
        assert_eq!(gate("x", 0).poll(interval).unwrap(), Turn::Go);
        // With the time read before the wait (100 s) it would still wait.
        assert_eq!(gate("late", 30_000).poll(interval).unwrap(), Turn::Go);
        assert_eq!(shared.borrow().last_switch_at, 130_000);
        // A poller that reads 4 s after x but gets the lock 1 s later waits
        // for the interval from its own grant, measured under the lock.
        let mut next = gate("next", 1_000);
        let turn = next.poll(interval).unwrap();
        assert_eq!(wait(turn).1, I - 1_000);
        assert_eq!(shared.borrow().queue[0].enqueued_at, 131_000);
    }
}
