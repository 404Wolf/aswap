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
//! queue, and a waiter that stops waiting removes its ticket.
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
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// The default `switch_interval_seconds`; 0 turns the gate off.
pub const DEFAULT_INTERVAL_SECONDS: u64 = 4;
/// How long past three intervals a silent waiter keeps its place, at most
/// (short intervals get five intervals of slack).
const SLACK_MS: u64 = 15_000;
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

/// A ticket not refreshed for this long belongs to a waiter that died.
pub fn ttl(interval_ms: u64) -> u64 {
    let slack = interval_ms.saturating_mul(5).min(SLACK_MS);
    interval_ms.saturating_mul(3).saturating_add(slack)
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
/// Timestamps ahead of `now` (a clock that stepped back) count as `now`, so a
/// future `last_switch_at` delays the next switch by one interval, never more.
pub fn decide(shared: &mut Shared, id: &str, now: u64, interval_ms: u64) -> (Turn, bool) {
    let before = shared.clone();
    if interval_ms == 0 {
        shared.queue.retain(|t| t.id != id);
        let changed = *shared != before;
        return (Turn::Go, changed);
    }
    shared.last_switch_at = shared.last_switch_at.min(now);
    let ttl = ttl(interval_ms);
    shared.queue.retain_mut(|t| {
        t.enqueued_at = t.enqueued_at.min(now);
        t.heartbeat_at = t.heartbeat_at.min(now);
        t.id == id || now - t.heartbeat_at <= ttl
    });
    let position = match shared.queue.iter().position(|t| t.id == id) {
        Some(position) => {
            let ticket = &mut shared.queue[position];
            if now - ticket.heartbeat_at >= heartbeat_every(interval_ms) {
                ticket.heartbeat_at = now;
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
        shared.last_switch_at = now;
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
impl Shelf for FileShelf {
    fn update<R>(&self, change: impl FnOnce(&mut Shared) -> (R, bool)) -> Result<R> {
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
        lock.lock_exclusive().safe("cannot lock the switch gate")?;
        // Unreadable or corrupt state only forgets the queue: at worst one
        // extra switch goes through early.
        let mut shared = match fs::symlink_metadata(&self.state) {
            Ok(_) => crate::store::read_private(&self.state)
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                .unwrap_or_default(),
            Err(_) => Shared::default(),
        };
        let (result, changed) = change(&mut shared);
        if changed {
            crate::store::json_write(&self.state, &shared)?;
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
        let now = self.clock.now_ms();
        let interval_ms = u64::try_from(interval.as_millis()).unwrap_or(u64::MAX);
        let id = &self.id;
        let turn = self
            .shelf
            .update(|shared| decide(shared, id, now, interval_ms))?;
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
        let mut reported = false;
        loop {
            match self.poll(interval) {
                Ok(Turn::Go) => return Ok(()),
                Ok(Turn::Wait {
                    position,
                    eta,
                    recheck,
                }) => {
                    if !reported {
                        eprintln!(
                            "aswap: waiting for the global switch slot: position {position}, ~{}s",
                            eta.as_millis().div_ceil(1000)
                        );
                        reported = true;
                    }
                    if let Err(err) = wait(recheck) {
                        self.leave();
                        return Err(err);
                    }
                }
                Err(err) => {
                    eprintln!("aswap: {err}; switching without the global switch slot");
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
}
