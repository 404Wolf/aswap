//! A single refresh/usage owner; followers receive access-only credentials over
//! an authenticated SSH pipe. There is no credential-bearing human output.
use crate::{
    Error, Result, Safe, now,
    provider::{self, Provider},
    store::{self, Account, Store},
    usage::{self, Cached},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, IsTerminal, Read, Write},
    os::unix::process::CommandExt,
    process::{Command, Stdio},
    time::{Duration, Instant},
};
mod log;
mod packet;
mod policy;
mod stream;
#[cfg(test)]
mod tests;
mod transport;

pub use log::recent_events;
use log::*;
use packet::*;
use policy::*;
#[cfg(test)]
use stream::*;
pub use stream::{serve, watch};
use transport::*;

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub owner: bool,
    pub host: Option<String>,
    pub last_sync: u64,
    /// When a follower last started an exchange, successful or not. Claimed
    /// under the store lock before the SSH round trip, so concurrent
    /// processes skip instead of each running their own sync.
    pub last_attempt: u64,
    /// Consecutive failed exchanges; widens the retry interval.
    pub failures: u32,
    /// The owner's `issued_at` of the installed packet: a slower, older
    /// exchange never overwrites a newer one.
    pub last_issued: u64,
    /// The push stream keeps its own schedule so a failing or hanging
    /// stream never delays the poll fallback: no new stream before this time.
    pub watch_next: u64,
    /// Consecutive push streams that failed before delivering a packet.
    pub watch_failures: u32,
}
/// A follower polls this long after its last delivery (poll or stream).
const POLL_EVERY: u64 = 30;
/// The longest a follower waits after a failed or still-running exchange.
/// Any sync can rotate away a revoked token, so this stays short.
const MAX_BACKOFF: u64 = 60;
/// When the next poll may start: 30s after the last delivery; after a failure
/// 30s and then 60s after the failed attempt; while an attempt is in flight
/// (claimed, not finished) 60s after its claim, so a hung exchange is retried
/// by someone else. The failure count is read but never trusted beyond that
/// cap: older releases sharing this file widen it to 300s on their own.
/// Times in the future (the clock stepped back) are ignored: clamping them to
/// now would slide the next poll forward forever.
fn next_poll(settings: &Settings, at: u64) -> u64 {
    let past = |t: u64| if t > at { 0 } else { t };
    let last_sync = past(settings.last_sync);
    let synced = last_sync.saturating_add(POLL_EVERY);
    let last_attempt = past(settings.last_attempt);
    if last_attempt <= last_sync {
        return synced;
    }
    let backoff = if settings.failures == 1 {
        POLL_EVERY
    } else {
        MAX_BACKOFF
    };
    synced.max(last_attempt.saturating_add(backoff))
}
fn due(settings: &Settings, at: u64) -> bool {
    at >= next_poll(settings, at)
}
/// The follower schedule and recent events for `status --json`.
pub fn status(store: &Store) -> Result<Value> {
    let s = settings(store)?;
    let at = now();
    Ok(json!({
        "last_sync": s.last_sync,
        "last_attempt": s.last_attempt,
        "failures": s.failures,
        "next_poll": s.host.is_some().then(|| next_poll(&s, at)),
        "watch_next": s.watch_next,
        "watch_failures": s.watch_failures,
        "events": recent_events(store, 10),
    }))
}
pub fn settings(store: &Store) -> Result<Settings> {
    let path = store.root.join("coordination.json");
    if path.exists() {
        store::json_read(&path)
    } else {
        Ok(Settings::default())
    }
}
pub fn is_owner(store: &Store) -> Result<bool> {
    Ok(settings(store)?.owner)
}
pub fn follower(store: &Store) -> Result<bool> {
    Ok(settings(store)?.host.is_some())
}
fn idle_profiles(store: &Store) -> Result<Vec<store::Guard>> {
    store
        .load()?
        .accounts
        .iter()
        .map(|a| {
            store
                .lease_label(a)
                .map_err(|_| Error("stop running profiles before changing refresh ownership"))
        })
        .collect()
}
pub fn owner(store: &Store) -> Result<()> {
    let _lock = store.lock()?;
    let _idle = if settings(store)?.host.is_some() {
        idle_profiles(store)?
    } else {
        Vec::new()
    };
    store::json_write(
        &store.root.join("coordination.json"),
        &Settings {
            owner: true,
            ..Settings::default()
        },
    )
}
pub fn off(store: &Store) -> Result<()> {
    let _lock = store.lock()?;
    let _idle = if settings(store)?.host.is_some() {
        idle_profiles(store)?
    } else {
        Vec::new()
    };
    store::json_write(&store.root.join("coordination.json"), &Settings::default())
}
fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 128
        && !host.starts_with('-')
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}
pub fn configure(store: &Store, host: &str) -> Result<usize> {
    if !valid_host(host) {
        return Err(Error("sync requires a plain SSH host alias"));
    }
    {
        let _lock = store.lock()?;
        let current = settings(store)?;
        if current.owner {
            return Err(Error(
                "sync: owner-role-conflict; use sync --off before becoming a follower",
            ));
        }
        let _idle = if current.host.is_none() {
            idle_profiles(store)?
        } else {
            Vec::new()
        };
        store::json_write(
            &store.root.join("coordination.json"),
            &Settings {
                owner: false,
                host: Some(host.into()),
                ..Settings::default()
            },
        )?;
    }
    poll(store, None, true)
}
/// One peer sync every 30s; all peers consume the owner's single persisted cache.
/// Caller must not already hold the store lock. The store lock is never held
/// across the SSH exchange: the attempt is claimed under it, the exchange runs
/// unlocked, and the packet is re-checked and installed under it.
pub fn poll(store: &Store, force: Option<&Account>, immediate: bool) -> Result<usize> {
    poll_with(store, force, immediate, exchange)
}
fn poll_with(
    store: &Store,
    force: Option<&Account>,
    immediate: bool,
    exchange: impl FnOnce(&str, &Request) -> Result<Packet>,
) -> Result<usize> {
    let bypass = immediate || force.is_some();
    let Some((host, failures)) = claim(store, |s, at| bypass || due(s, at))? else {
        return Ok(0);
    };
    log(store, "poll", "claim", None, None);
    let result = exchange(
        &host,
        &Request {
            protocol: 1,
            force: force.cloned(),
        },
    );
    let _lock = store.lock()?;
    if settings(store)?.host.as_deref() != Some(host.as_str()) {
        return Err(Error("sync: configuration changed during sync"));
    }
    match result.and_then(|packet| accept(store, packet)) {
        Ok(done) => {
            log(store, "poll", done.event, None, Some(done.count));
            if immediate && done.duplicates > 0 {
                eprintln!(
                    "aswap sync: duplicate-login warning: {} duplicate profile(s); aliases retained",
                    done.duplicates
                );
            }
            Ok(done.count)
        }
        Err(err) => {
            record_failure(store, failures)?;
            log(store, "poll", "fail", Some(code(&err)), None);
            Err(err)
        }
    }
}
/// Claim an exchange under the store lock when `ready` says one may start:
/// returns the owner host and the failure count before this attempt.
fn claim(store: &Store, ready: impl Fn(&Settings, u64) -> bool) -> Result<Option<(String, u32)>> {
    let initial = settings(store)?;
    if initial.host.is_none() || !ready(&initial, now()) {
        return Ok(None);
    }
    let _lock = store.lock()?;
    let mut settings = settings(store)?;
    let Some(host) = settings.host.clone() else {
        return Ok(None);
    };
    if !ready(&settings, now()) {
        return Ok(None);
    }
    settings.last_attempt = now();
    store::json_write(&store.root.join("coordination.json"), &settings)?;
    Ok(Some((host, settings.failures)))
}
/// What a delivered packet did: `event` is installed (some auth changed),
/// unchanged, or stale (a newer packet had already landed).
struct Accepted {
    count: usize,
    duplicates: usize,
    event: &'static str,
}
/// Two overlapping exchanges are at most this far apart on the owner's clock.
/// A packet older than the installed one by more than this means the owner's
/// clock stepped back; it is installed rather than skipped forever.
const OVERLAP: u64 = 300;
/// Caller holds the store lock and has checked the host. Installs a packet
/// unless a newer one already landed.
fn accept(store: &Store, packet: Packet) -> Result<Accepted> {
    let policy = SyncPolicy::read(&store.sync_policy_path)?;
    policy.check_local(store)?;
    let mut settings = settings(store)?;
    if packet.issued_at < settings.last_issued && settings.last_issued - packet.issued_at <= OVERLAP
    {
        // The owner answered and something at least as new is installed:
        // that is a delivery, not an attempt still in flight.
        settings.failures = 0;
        settings.last_sync = now();
        store::json_write(&store.root.join("coordination.json"), &settings)?;
        return Ok(Accepted {
            count: packet
                .profiles
                .iter()
                .filter(|p| policy.permits(&p.account))
                .count(),
            duplicates: 0,
            event: "stale",
        });
    }
    let duplicates = duplicate_count_permitted(&packet, &policy);
    settings.failures = 0;
    settings.last_issued = packet.issued_at;
    let (count, changed) = install_permitted(store, packet, &mut settings, &policy)?;
    Ok(Accepted {
        count,
        duplicates,
        event: if changed { "installed" } else { "unchanged" },
    })
}
/// Caller holds the store lock.
fn record_failure(store: &Store, failures: u32) -> Result<()> {
    let mut settings = settings(store)?;
    settings.failures = failures.saturating_add(1);
    settings.last_attempt = now();
    store::json_write(&store.root.join("coordination.json"), &settings)
}
/// How often the fallback poller checks whether a poll is due.
const POLL_CHECK: Duration = Duration::from_secs(5);
/// Start a follower's sync helpers: the push stream (whoever takes the watch
/// lock first streams; the others keep trying so the stream moves to a
/// survivor when that process exits) and the fallback poller. Every caller
/// runs a poller, and `due()` plus the claim under the store lock make one
/// of them poll 30s after the last delivery, so a stream that is down,
/// flapping or silent never leaves the follower waiting on the monitor's
/// slow usage tick or on older releases' backoff.
pub fn spawn_sync(store: &Store) {
    if !follower(store).unwrap_or(false) {
        return;
    }
    let (Ok(watching), Ok(polling)) = (
        Store::open(store.root.clone()),
        Store::open(store.root.clone()),
    ) else {
        return;
    };
    std::thread::spawn(move || {
        while follower(&watching).unwrap_or(false) {
            let _ = watch(&watching);
            std::thread::sleep(Duration::from_secs(5));
        }
    });
    std::thread::spawn(move || poll_loop(&polling, POLL_CHECK, || poll(&polling, None, false)));
}
/// Run `poll` every `every` while this store is a follower. `poll` only
/// exchanges when one is due, so this costs a file read per tick otherwise.
fn poll_loop(store: &Store, every: Duration, mut poll: impl FnMut() -> Result<usize>) {
    while follower(store).unwrap_or(false) {
        let _ = poll();
        std::thread::sleep(every);
    }
}
