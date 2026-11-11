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
/// Write `value` only when the file does not already hold it: heartbeat
/// packets must leave profile auth stamps alone, or every running view would
/// resync on each heartbeat.
fn write_changed<T: Serialize>(path: &std::path::Path, value: &T) -> Result<bool> {
    let value = serde_json::to_value(value).safe("cannot encode private state")?;
    if path.exists() && store::json_read::<Value>(path).ok().as_ref() == Some(&value) {
        return Ok(false);
    }
    store::json_write(path, &value).map(|()| true)
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
/// Private, size-capped sync event log: one JSON line per event with the
/// schedule fields. Only fixed application codes, counts and times; never a
/// packet, token, label, email, host or path.
const LOG: &str = "sync.log";
const LOG_LIMIT: u64 = 1024 * 1024;
fn code(error: &Error) -> &'static str {
    let text = error.0.strip_prefix("sync: ").unwrap_or(error.0);
    text.split(';').next().unwrap_or(text)
}
fn log(store: &Store, kind: &str, event: &str, code: Option<&str>, profiles: Option<usize>) {
    let at = now();
    let s = settings(store).unwrap_or_default();
    let line = json!({
        "at": at,
        "pid": std::process::id(),
        "kind": kind,
        "event": event,
        "code": code,
        "profiles": profiles,
        "last_sync": s.last_sync,
        "last_attempt": s.last_attempt,
        "failures": s.failures,
        "next_poll": next_poll(&s, at),
        "watch_next": s.watch_next,
        "watch_failures": s.watch_failures,
    });
    let _ = append_log(&store.root.join(LOG), &line);
}
fn append_log(path: &std::path::Path, line: &Value) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    store::no_links(path)?;
    if std::fs::metadata(path).is_ok_and(|m| m.len() >= LOG_LIMIT) {
        let old = path.with_extension("log.1");
        store::no_links(&old)?;
        std::fs::rename(path, &old).safe("cannot rotate sync log")?;
    }
    let mut bytes = serde_json::to_vec(line).safe("cannot encode sync log")?;
    bytes.push(b'\n');
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .safe("cannot open sync log")?;
    // One write per line: concurrent appenders never interleave a line.
    file.write_all(&bytes).safe("cannot write sync log")
}
/// The newest `count` sync events, oldest first, for `status --json`.
pub fn recent_events(store: &Store, count: usize) -> Vec<Value> {
    let Ok(bytes) = store::read_private(&store.root.join(LOG)) else {
        return Vec::new();
    };
    let mut events: Vec<Value> = bytes
        .split(|b| *b == b'\n')
        .rev()
        .filter_map(|line| serde_json::from_slice(line).ok())
        .take(count)
        .collect();
    events.reverse();
    events
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
// These are transport-only types. Never derive Debug, format packets in errors,
// store them in logs, or return them from an ordinary CLI JSON command.
#[derive(Serialize, Deserialize)]
struct Request {
    protocol: u8,
    force: Option<Account>,
}
#[derive(Serialize, Deserialize)]
struct Profile {
    account: Account,
    auth: Value,
    config: Value,
    cache: Cached,
}
#[derive(Serialize, Deserialize)]
struct Packet {
    protocol: u8,
    issued_at: u64,
    profiles: Vec<Profile>,
}
fn packet(store: &Store, request: &Request) -> Result<Packet> {
    if request.protocol != 1 {
        return Err(Error("sync: version-mismatch"));
    }
    if !is_owner(store)? || follower(store)? {
        return Err(Error(
            "sync: owner-not-configured; run sync --owner on the server using its service store",
        ));
    }
    let accounts = {
        let _lock = store.lock()?;
        store.load()?.accounts
    };
    let mut profiles = Vec::new();
    for account in accounts {
        let force = request
            .force
            .as_ref()
            .is_some_and(|a| a.provider == account.provider && a.label == account.label);
        let cache = usage::cached_force(store, &account, force)?;
        let _lock = store.lock()?;
        let path = store.profile(account.provider, &account.label)?;
        let auth: Value = store::json_read(&path.join(account.provider.auth_file()))?;
        let config = if account.provider == Provider::Claude && path.join(".claude.json").exists() {
            crate::shared::account_config(&store::json_read(&path.join(".claude.json"))?)
        } else {
            json!({})
        };
        profiles.push(Profile {
            auth: crate::refresh::access_only(account.provider, &auth),
            account,
            config,
            cache,
        });
    }
    Ok(Packet {
        protocol: 1,
        issued_at: now(),
        profiles,
    })
}
fn install(store: &Store, packet: Packet, settings: &mut Settings) -> Result<(usize, bool)> {
    if packet.protocol != 1 {
        return Err(Error("sync: version-mismatch"));
    }
    if packet.profiles.len() > 128 {
        return Err(Error("sync: bad-response"));
    }
    let mut state = store.load()?;
    let original = serde_json::to_value(&state).safe("cannot encode private state")?;
    let mut seen = std::collections::BTreeSet::new();
    // Validate the whole packet before writing any account.
    for p in &packet.profiles {
        store::label(&p.account.label).map_err(|_| Error("sync: invalid-label"))?;
        if !seen.insert((p.account.provider, p.account.label.clone())) {
            return Err(Error(
                "sync: label-conflict; duplicate provider/label in owner packet",
            ));
        }
        provider::validate_auth(
            p.account.provider,
            &serde_json::to_vec(&p.auth).safe("invalid owner authentication")?,
        )
        .map_err(|_| Error("sync: bad-response; invalid owner authentication"))?;
        let path = store.profile(p.account.provider, &p.account.label)?;
        if path.join(p.account.provider.auth_file()).exists() {
            let local = crate::identity::read(p.account.provider, &path).unwrap_or_default();
            let remote = crate::identity::from_values(p.account.provider, &p.auth, &p.config);
            if local.known() && remote.known() && !crate::identity::same(&local, &remote) {
                return Err(Error(
                    "sync: identity-conflict; same label has a different account; align labels first",
                ));
            }
        }
    }
    let count = packet.profiles.len();
    let mut changed = false;
    for p in packet.profiles {
        let path = store.profile(p.account.provider, &p.account.label)?;
        store::private_dir(&path)?;
        changed |= write_changed(
            &path.join(p.account.provider.auth_file()),
            &crate::refresh::access_only(p.account.provider, &p.auth),
        )?;
        if p.account.provider == Provider::Claude {
            write_changed(
                &path.join(".claude.json"),
                &crate::shared::account_config(&p.config),
            )?;
        }
        write_changed(&path.join("usage-cache.json"), &p.cache)?;
        if !state
            .accounts
            .iter()
            .any(|a| a.provider == p.account.provider && a.label == p.account.label)
        {
            state.accounts.push(p.account.clone());
        }
        state
            .active
            .entry(p.account.provider)
            .or_insert(p.account.label);
    }
    for account in &state.accounts {
        if !seen.contains(&(account.provider, account.label.clone())) {
            let path = store
                .profile(account.provider, &account.label)?
                .join("usage-cache.json");
            let mut cache: Cached = if path.exists() {
                store::json_read(&path)?
            } else {
                Cached::default()
            };
            cache.error = Some("owner-profile-missing".into());
            cache.http_status = None;
            write_changed(&path, &cache)?;
        }
    }
    if serde_json::to_value(&state).safe("cannot encode private state")? != original {
        store.save(&state)?;
    }
    // Peer scheduling uses the local clock, never the owner's clock as proof
    // of credential validity or freshness. SSH pulls are serialized locally.
    settings.last_sync = now();
    store::json_write(&store.root.join("coordination.json"), settings)?;
    Ok((count, changed))
}
// Only exact, application-owned messages survive the first-line filter. This is
// deliberately not token-pattern replacement: an unknown diagnostic could contain
// an email, path, token, terminal controls or an entire credential JSON object.
fn remote_line(bytes: &[u8]) -> &'static str {
    let Ok(line) = std::str::from_utf8(bytes) else {
        return "redacted-unrecognized";
    };
    let line = line.trim_end_matches(['\r', '\n']);
    match line {
        "aswap: sync server requires the configured refresh owner"
        | "aswap: sync: owner-not-configured; run sync --owner on the server using its service store" => {
            "owner-not-configured"
        }
        "aswap: sync: version-mismatch" => "version-mismatch",
        "aswap: invalid sync request" | "aswap: sync: invalid-request" => "invalid-request",
        "aswap: sync transport is restricted to an SSH pipe" => "transport-restricted",
        "aswap: owner profile identity mismatch; align profile labels first"
        | "aswap: sync: identity-conflict; same label has a different account; align labels first" => {
            "identity-conflict"
        }
        "aswap: duplicate account in owner packet"
        | "aswap: sync: label-conflict; duplicate provider/label in owner packet" => {
            "label-conflict"
        }
        "aswap: cannot read private file"
        | "aswap: invalid private JSON; contents redacted"
        | "aswap: cannot encode private state"
        | "aswap: cannot secure directory"
        | "aswap: cannot create private directory"
        | "aswap: cannot inspect private path"
        | "aswap: private file exceeds size limit"
        | "aswap: expected a regular private file"
        | "aswap: sync: owner-store-error" => "owner-store-error",
        "aswap: refusing a symlink or managed file" => "owner-managed-path",
        "aswap: unsupported or missing credentials; use a file-based provider login first"
        | "aswap: invalid credential JSON; contents redacted" => "owner-auth-invalid",
        "aswap: sync: timeout" => "timeout",
        "aswap: sync: bad-response" => "bad-response",
        "aswap: cannot send owner packet" => "owner-write-failed",
        "aswap: sync request too large" => "request-too-large",
        "aswap: cannot read sync request" => "owner-read-failed",
        _ => "redacted-unrecognized",
    }
}
fn remote_error(status: std::process::ExitStatus, first: &'static str) -> Error {
    if let Some(code) = status.code() {
        eprintln!("aswap sync: remote-exit-{code}; remote-stderr-first-line: {first}");
    } else {
        eprintln!("aswap sync: remote-signaled; remote-stderr-first-line: {first}");
    }
    Error(match first {
        "owner-not-configured" => {
            "sync: owner-not-configured; check the SSH user's role/store; run sync --owner there"
        }
        "version-mismatch" => "sync: version-mismatch",
        "identity-conflict" => "sync: identity-conflict",
        "label-conflict" => "sync: label-conflict",
        "invalid-request" => "sync: invalid-request",
        "owner-store-error" => "sync: owner-store-error",
        "owner-managed-path" => "sync: owner-managed-path",
        "owner-auth-invalid" => "sync: owner-auth-invalid",
        "transport-restricted" => "sync: transport-restricted",
        "timeout" => "sync: timeout",
        "bad-response" => "sync: bad-response",
        _ => "sync: remote-exit; see classified remote stderr above",
    })
}
fn exchange(host: &str, request: &Request) -> Result<Packet> {
    exchange_command(
        ssh(host, "aswap sync --serve")?,
        request,
        Duration::from_secs(90),
    )
}
fn exchange_command(mut command: Command, request: &Request, timeout: Duration) -> Result<Packet> {
    let mut child = command
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .safe("sync: ssh-start-failed")?;
    let result = (|| {
        let mut input = child
            .stdin
            .take()
            .ok_or(Error("sync: request-write-failed"))?;
        let sent =
            serde_json::to_writer(&mut input, request).is_ok() && input.write_all(b"\n").is_ok();
        drop(input);
        let output = child.stdout.take().ok_or(Error("sync: bad-response"))?;
        let err_rx = stderr_filter(child.stderr.take().ok_or(Error("sync: bad-response"))?);
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let result = output.take(4 * 1024 * 1024 + 1).read_to_end(&mut bytes);
            let _ = tx.send((result.is_ok(), bytes));
        });
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some(status) = child.try_wait().safe("sync: ssh-wait-failed")? {
                if !status.success() {
                    return Err(remote_error(
                        status,
                        err_rx
                            .recv_timeout(Duration::from_secs(1))
                            .unwrap_or("unavailable"),
                    ));
                }
                // Judge the response first: a peer that exits without reading
                // stdin races the request write (EPIPE).
                let (ok, bytes) = rx
                    .recv_timeout(Duration::from_secs(1))
                    .safe("sync: bad-response; incomplete SSH output")?;
                if !ok || bytes.len() > 4 * 1024 * 1024 {
                    return Err(Error("sync: bad-response; invalid response size"));
                }
                // A complete packet is authoritative (protocol checks follow);
                // a failed write only explains a peer that said nothing.
                if bytes.is_empty() && !sent {
                    return Err(Error("sync: request-write-failed"));
                }
                return serde_json::from_slice(&bytes)
                    .safe("sync: bad-response; invalid protocol JSON");
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        Err(Error("sync: timeout"))
    })();
    if result.is_err() || child.try_wait().ok().flatten().is_none() {
        // SAFETY: this child was created as the owner of a private process group.
        // Also stop surviving descendants holding pipes after the SSH child exits.
        unsafe {
            libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
        }
    }
    let _ = child.wait();
    result
}
fn duplicate_count(packet: &Packet) -> usize {
    let mut seen: Vec<(Provider, crate::identity::Identity)> = Vec::new();
    let mut count = 0;
    for p in &packet.profiles {
        let identity = crate::identity::from_values(p.account.provider, &p.auth, &p.config);
        if seen.iter().any(|(provider, prior)| {
            *provider == p.account.provider && crate::identity::same(prior, &identity)
        }) {
            count += 1;
        } else {
            seen.push((p.account.provider, identity));
        }
    }
    count
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
    let mut settings = settings(store)?;
    if packet.issued_at < settings.last_issued && settings.last_issued - packet.issued_at <= OVERLAP
    {
        // The owner answered and something at least as new is installed:
        // that is a delivery, not an attempt still in flight.
        settings.failures = 0;
        settings.last_sync = now();
        store::json_write(&store.root.join("coordination.json"), &settings)?;
        return Ok(Accepted {
            count: packet.profiles.len(),
            duplicates: 0,
            event: "stale",
        });
    }
    let duplicates = duplicate_count(&packet);
    settings.failures = 0;
    settings.last_issued = packet.issued_at;
    let (count, changed) = install(store, packet, &mut settings)?;
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
/// Request protocol of the push stream (`sync --serve --watch`): the owner
/// answers with newline-separated protocol-1 packets, one at start, one after
/// every profile auth change and a heartbeat every `HEARTBEAT`.
const WATCH_PROTOCOL: u8 = 2;
const HEARTBEAT: Duration = Duration::from_secs(20);
/// A stream that stays silent for this long (three heartbeats) is dead.
const STREAM_IDLE: Duration = Duration::from_secs(60);
const PACKET_LIMIT: usize = 4 * 1024 * 1024;
fn ssh(host: &str, remote: &str) -> Result<Command> {
    if !valid_host(host) {
        return Err(Error("invalid sync host alias"));
    }
    let mut command = Command::new("ssh");
    command.args([
        "-T",
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=5",
        "-o",
        "ServerAliveInterval=10",
        "-o",
        "ServerAliveCountMax=2",
        "--",
        host,
        remote,
    ]);
    Ok(command)
}
/// Classify the first stderr line on a helper thread and drain the rest, so
/// verbose SSH/provider errors cannot deadlock the pipe.
fn stderr_filter(stderr: std::process::ChildStderr) -> std::sync::mpsc::Receiver<&'static str> {
    let (err_tx, err_rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut first = Vec::new();
        let ok = reader
            .by_ref()
            .take(4097)
            .read_until(b'\n', &mut first)
            .is_ok();
        let filtered = if !ok || first.len() > 4096 {
            "redacted-unrecognized"
        } else if first.is_empty() {
            "empty"
        } else {
            remote_line(&first)
        };
        let _ = err_tx.send(filtered);
        let _ = std::io::copy(&mut reader, &mut std::io::sink());
    });
    err_rx
}
enum Frame {
    Line(Vec<u8>),
    End,
    Bad,
}
/// Read one owner stream. Each packet goes to `on_packet` as it arrives;
/// `stop` ends the stream early. Returns how many packets were accepted and
/// how the stream ended. The child is always killed and reaped.
fn stream_command(
    mut command: Command,
    request: &Request,
    first: Duration,
    idle: Duration,
    mut on_packet: impl FnMut(Packet) -> Result<()>,
    mut stop: impl FnMut() -> bool,
) -> (usize, Result<()>) {
    // SAFETY: prctl is async-signal-safe. The stream outlives no watcher: if
    // this process dies, the SSH child (and its pipes) die with it.
    unsafe {
        command.pre_exec(|| {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = match command
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return (0, Err(Error("sync: ssh-start-failed"))),
    };
    let mut accepted = 0;
    let result = (|| {
        // Keep stdin open: the owner ends its stream when it reaches EOF.
        let mut input = child
            .stdin
            .take()
            .ok_or(Error("sync: request-write-failed"))?;
        serde_json::to_writer(&mut input, request).safe("sync: request-write-failed")?;
        input.write_all(b"\n").safe("sync: request-write-failed")?;
        input.flush().safe("sync: request-write-failed")?;
        let output = child.stdout.take().ok_or(Error("sync: bad-response"))?;
        let err_rx = stderr_filter(child.stderr.take().ok_or(Error("sync: bad-response"))?);
        // Bounded: the reader waits for each packet to be installed.
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let mut reader = BufReader::new(output);
            loop {
                let mut line = Vec::new();
                let frame = match reader
                    .by_ref()
                    .take(PACKET_LIMIT as u64 + 2)
                    .read_until(b'\n', &mut line)
                {
                    Ok(0) => Frame::End,
                    Ok(_) if line.len() > PACKET_LIMIT + 1 || line.last() != Some(&b'\n') => {
                        Frame::Bad
                    }
                    Ok(_) => Frame::Line(line),
                    Err(_) => Frame::Bad,
                };
                let last = !matches!(frame, Frame::Line(_));
                if tx.send(frame).is_err() || last {
                    return;
                }
            }
        });
        let mut heard = Instant::now();
        loop {
            if stop() {
                return Ok(());
            }
            match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(Frame::Line(bytes)) => {
                    let packet: Packet = serde_json::from_slice(&bytes)
                        .safe("sync: bad-response; invalid protocol JSON")?;
                    on_packet(packet)?;
                    accepted += 1;
                    heard = Instant::now();
                }
                Ok(Frame::Bad) => return Err(Error("sync: bad-response; invalid stream frame")),
                Ok(Frame::End) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    let deadline = Instant::now() + Duration::from_secs(1);
                    while Instant::now() < deadline {
                        if let Some(status) = child.try_wait().safe("sync: ssh-wait-failed")? {
                            if !status.success() {
                                return Err(remote_error(
                                    status,
                                    err_rx
                                        .recv_timeout(Duration::from_secs(1))
                                        .unwrap_or("unavailable"),
                                ));
                            }
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(25));
                    }
                    return Err(Error("sync: stream-ended"));
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if accepted == 0 && heard.elapsed() >= first {
                        return Err(Error(NO_STREAM));
                    }
                    if heard.elapsed() >= idle {
                        return Err(Error("sync: timeout"));
                    }
                }
            }
        }
    })();
    // SAFETY: this child was created as the owner of a private process group.
    unsafe {
        libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
    }
    let _ = child.wait();
    (accepted, result)
}
/// An owner that sends nothing this long after connecting cannot stream
/// (an older release waits for stdin EOF that never comes).
/// A current owner under load can take seconds to build its first packet
/// (it reads every profile's usage cache); don't mistake that for an old one.
const FIRST_PACKET: Duration = Duration::from_secs(30);
const NO_STREAM: &str = "sync: no-stream; owner sent no first packet";
/// One follower connection to the owner's push stream. It has its own
/// schedule (`watch_next`, `watch_failures`) and never touches the poll's
/// `last_attempt`/`failures`, so the 30s poll fallback keeps its cadence
/// however the stream fares. Packets are installed under the store lock as
/// they arrive; the lock is never held while waiting on the network. A
/// delivered packet counts as a sync (last_sync, last_issued, poll failures).
fn watch_session(
    store: &Store,
    command: impl FnOnce(&str) -> Result<Command>,
    first: Duration,
    idle: Duration,
) -> Result<Option<usize>> {
    let host = {
        let _lock = store.lock()?;
        let mut settings = settings(store)?;
        let Some(host) = settings.host.clone() else {
            return Ok(None);
        };
        if now() < settings.watch_next {
            return Ok(None);
        }
        // A flapping owner gets at most one new stream per poll interval.
        settings.watch_next = now().saturating_add(30);
        store::json_write(&store.root.join("coordination.json"), &settings)?;
        log(store, "stream", "connect", None, None);
        host
    };
    let moved = || settings(store).map_or(true, |s| s.host.as_deref() != Some(host.as_str()));
    let mut refused = false;
    let (accepted, result) = match command(&host) {
        Ok(command) => stream_command(
            command,
            &Request {
                protocol: WATCH_PROTOCOL,
                force: None,
            },
            first,
            idle,
            |packet| {
                let _lock = store.lock()?;
                if moved() {
                    return Err(Error("sync: configuration changed during sync"));
                }
                accept(store, packet)
                    .map(|done| {
                        // Heartbeats every 20s would flood the log: record
                        // only packets that changed something.
                        if done.event != "unchanged" {
                            log(store, "stream", done.event, None, Some(done.count));
                        }
                    })
                    .inspect_err(|_| refused = true)
            },
            moved,
        ),
        Err(err) => (0, Err(err)),
    };
    let _lock = store.lock()?;
    if moved() {
        return Ok(Some(accepted));
    }
    let mut settings = settings(store)?;
    match result {
        Err(err) if accepted == 0 || refused => {
            settings.watch_failures = settings.watch_failures.saturating_add(1);
            let delay = if err.0 == NO_STREAM {
                600
            } else {
                (60u64 << settings.watch_failures.min(4)).min(600)
            };
            settings.watch_next = settings.watch_next.max(now().saturating_add(delay));
            store::json_write(&store.root.join("coordination.json"), &settings)?;
            log(store, "stream", "fail", Some(code(&err)), Some(accepted));
            Err(err)
        }
        result => {
            settings.watch_failures = 0;
            store::json_write(&store.root.join("coordination.json"), &settings)?;
            let code = result.err().map(|e| code(&e));
            log(store, "stream", "end", code, Some(accepted));
            Ok(Some(accepted))
        }
    }
}
/// Keep the owner's push stream open for this follower store until it stops
/// being a follower. Only one process per store streams; the others return
/// at once and rely on the 30s poll, which stays the fallback.
pub fn watch(store: &Store) -> Result<()> {
    let Some(_watching) = store.watch_lock()? else {
        return Ok(());
    };
    while follower(store)? {
        // Stay quiet: this runs beside a provider's terminal UI; the stream
        // schedules its own retries in coordination.json.
        let _ = watch_session(
            store,
            |host| ssh(host, "aswap sync --serve --watch"),
            FIRST_PACKET,
            STREAM_IDLE,
        );
        std::thread::sleep(Duration::from_secs(1));
    }
    Ok(())
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
/// Hidden SSH-only transport; refuse credential output to a terminal or a local
/// invocation. Normal `sync --json` returns counts/role, never this packet.
pub fn serve(store: &Store, watch: bool) -> Result<()> {
    if std::io::stdout().is_terminal()
        || std::io::stdin().is_terminal()
        || std::env::var_os("SSH_CONNECTION").is_none()
    {
        return Err(Error("sync transport is restricted to an SSH pipe"));
    }
    if watch {
        return serve_watch(store);
    }
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(8193)
        .read_to_end(&mut bytes)
        .safe("cannot read sync request")?;
    if bytes.len() > 8192 {
        return Err(Error("sync request too large"));
    }
    let request: Request = serde_json::from_slice(&bytes).safe("sync: invalid-request")?;
    let packet = packet(store, &request).map_err(owner_packet_error)?;
    let mut output = std::io::stdout().lock();
    serde_json::to_writer(&mut output, &packet).safe("cannot send owner packet")?;
    output.flush().safe("cannot send owner packet")
}
fn owner_packet_error(e: Error) -> Error {
    if e.0.starts_with("sync:") {
        e
    } else {
        Error("sync: owner-store-error")
    }
}
/// What a follower's packet depends on: the account list and every
/// profile's auth file. Stats only; no store lock and no file contents.
fn auth_print(store: &Store) -> Vec<Option<crate::shared::Stamp>> {
    let mut print = vec![crate::shared::stamp(&store.root.join("state.json"))];
    for account in store.load().map(|s| s.accounts).unwrap_or_default() {
        print.push(
            store
                .profile(account.provider, &account.label)
                .ok()
                .and_then(|p| crate::shared::stamp(&p.join(account.provider.auth_file()))),
        );
    }
    print
}
/// The push stream: one packet per line at start, within a tick of any
/// profile auth change (a refresh revokes the old access token at once), and
/// a heartbeat every `HEARTBEAT`. Ends when the follower closes stdin or the
/// pipe breaks.
fn serve_watch(store: &Store) -> Result<()> {
    let mut input = BufReader::new(std::io::stdin());
    let mut line = Vec::new();
    input
        .by_ref()
        .take(8193)
        .read_until(b'\n', &mut line)
        .safe("cannot read sync request")?;
    if line.len() > 8192 {
        return Err(Error("sync request too large"));
    }
    let request: Request = serde_json::from_slice(&line).safe("sync: invalid-request")?;
    if request.protocol != WATCH_PROTOCOL || request.force.is_some() {
        return Err(Error("sync: version-mismatch"));
    }
    let closed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let closed = closed.clone();
        std::thread::spawn(move || {
            let _ = std::io::copy(&mut input, &mut std::io::sink());
            closed.store(true, std::sync::atomic::Ordering::SeqCst);
        });
    }
    let single = Request {
        protocol: 1,
        force: None,
    };
    let mut sent: Option<(Vec<Option<crate::shared::Stamp>>, Instant)> = None;
    while !closed.load(std::sync::atomic::Ordering::SeqCst) {
        // Stamp before building: a change during the build sends again.
        let print = auth_print(store);
        if sent
            .as_ref()
            .is_none_or(|(last, at)| *last != print || at.elapsed() >= HEARTBEAT)
        {
            let packet = packet(store, &single).map_err(owner_packet_error)?;
            let mut bytes = serde_json::to_vec(&packet).safe("cannot send owner packet")?;
            drop(packet);
            bytes.push(b'\n');
            let mut output = std::io::stdout().lock();
            match output.write_all(&bytes).and_then(|_| output.flush()) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => return Ok(()),
                Err(_) => return Err(Error("cannot send owner packet")),
            }
            sent = Some((print, Instant::now()));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn seed() -> (tempfile::TempDir, Store) {
        let t = tempfile::tempdir().unwrap();
        let store = Store::open(t.path().join("store")).unwrap();
        let a = Account {
            provider: Provider::Claude,
            label: "one".into(),
        };
        let mut state = store.load().unwrap();
        state.accounts.push(a.clone());
        store.save(&state).unwrap();
        let profile = store.profile(a.provider, &a.label).unwrap();
        store::json_write(&profile.join(".credentials.json"),&json!({"claudeAiOauth":{"accessToken":"synthetic-owner-access","refreshToken":"synthetic-owner-refresh","expiresAt":9999999999999u64}})).unwrap();
        store::json_write(&profile.join(".claude.json"),&json!({"oauthAccount":{"accountUuid":"synthetic-owner"},"projects":{"synthetic-private-project":true}})).unwrap();
        let cached = Cached {
            usage: usage::Usage {
                windows: vec![usage::Window {
                    name: "five_hour".into(),
                    used_percent: 42.0,
                    resets_at: None,
                    duration_minutes: Some(300),
                }],
                ..Default::default()
            },
            fetched_at: now(),
            next_poll_at: now() + 3600,
            ..Default::default()
        };
        store::json_write(&profile.join("usage-cache.json"), &cached).unwrap();
        owner(&store).unwrap();
        (t, store)
    }
    #[test]
    fn owner_packets_share_one_cache_and_never_export_refresh_capability_or_project_state() {
        let (_t, store) = seed();
        let request = Request {
            protocol: 1,
            force: None,
        };
        let one = packet(&store, &request).unwrap();
        let two = packet(&store, &request).unwrap();
        assert_eq!(
            one.profiles[0].cache.fetched_at,
            two.profiles[0].cache.fetched_at
        );
        let encoded = serde_json::to_vec(&one).unwrap();
        let text = String::from_utf8(encoded.clone()).unwrap();
        assert!(!text.contains("synthetic-owner-refresh"));
        assert!(!text.contains("synthetic-private-project"));
        let t = tempfile::tempdir().unwrap();
        let peer = Store::open(t.path().join("peer")).unwrap();
        let mut cfg = Settings {
            host: Some("synthetic-owner".into()),
            ..Default::default()
        };
        assert_eq!(
            install(&peer, serde_json::from_slice(&encoded).unwrap(), &mut cfg)
                .unwrap()
                .0,
            1
        );
        let auth: Value = store::json_read(
            &peer
                .profile(Provider::Claude, "one")
                .unwrap()
                .join(".credentials.json"),
        )
        .unwrap();
        assert!(auth["claudeAiOauth"].get("refreshToken").is_none());
        assert!(!is_owner(&peer).unwrap());
        let a = Account {
            provider: Provider::Claude,
            label: "one".into(),
        };
        let cache = usage::cached_force(&peer, &a, false).unwrap();
        assert_eq!(cache.used(now()), Some(42.0));
        // Followers consume imported cached quota and never invoke HTTP/refresh.
        assert!(
            !crate::refresh::refresh_with(&peer, &a, 99999999999, true, |_, _| panic!(
                "follower must never rotate"
            ))
            .unwrap()
        );
        let mut collision = packet(&store, &request).unwrap();
        collision.profiles[0].config =
            json!({"oauthAccount":{"accountUuid":"synthetic-different"}});
        assert!(install(&peer, collision, &mut cfg).is_err());
    }
    #[test]
    fn hosts_and_transport_cannot_be_injected_or_used_as_human_credential_output() {
        for bad in [
            "",
            "-oProxyCommand=synthetic",
            "synthetic;touch",
            "synthetic user",
            "user@example.com",
            "x\ncommand",
        ] {
            assert!(!valid_host(bad));
        }
        assert!(valid_host("synthetic-owner"));
        let (_t, store) = seed();
        assert!(serve(&store, false).is_err());
        assert!(serve(&store, true).is_err());
    }
    #[test]
    fn duplicate_identities_are_aliases_but_duplicate_labels_and_identity_changes_are_conflicts() {
        let (_t, store) = seed();
        let source = store.profile(Provider::Claude, "one").unwrap();
        let target = store.profile(Provider::Claude, "alias").unwrap();
        for file in [".credentials.json", ".claude.json", "usage-cache.json"] {
            let value: Value = store::json_read(&source.join(file)).unwrap();
            store::json_write(&target.join(file), &value).unwrap();
        }
        let mut state = store.load().unwrap();
        state.accounts.push(Account {
            provider: Provider::Claude,
            label: "alias".into(),
        });
        store.save(&state).unwrap();
        let request = Request {
            protocol: 1,
            force: None,
        };
        let p = packet(&store, &request).unwrap();
        assert_eq!(duplicate_count(&p), 1);
        let peer_dir = tempfile::tempdir().unwrap();
        let peer = Store::open(peer_dir.path().join("peer")).unwrap();
        let mut config = Settings::default();
        assert_eq!(install(&peer, p, &mut config).unwrap().0, 2);
        assert_eq!(peer.load().unwrap().accounts.len(), 2);
        let mut p = packet(&store, &request).unwrap();
        p.profiles[1].account.label = p.profiles[0].account.label.clone();
        assert!(
            install(&peer, p, &mut config)
                .unwrap_err()
                .0
                .contains("label-conflict")
        );
        let mut p = packet(&store, &request).unwrap();
        p.profiles[1].config = json!({"oauthAccount":{"accountUuid":"synthetic-other"}});
        assert!(
            install(&peer, p, &mut config)
                .unwrap_err()
                .0
                .contains("identity-conflict")
        );
        // Preflight failures do not replace either alias's credentials.
        let current = crate::identity::read(
            Provider::Claude,
            &peer.profile(Provider::Claude, "alias").unwrap(),
        )
        .unwrap();
        let original = crate::identity::read(Provider::Claude, &source).unwrap();
        assert!(crate::identity::same(&current, &original));
    }
    #[test]
    fn owner_role_and_protocol_are_distinct_and_owner_cannot_be_silently_demoted() {
        let (_t, store) = seed();
        let err = packet(
            &store,
            &Request {
                protocol: 2,
                force: None,
            },
        )
        .err()
        .unwrap();
        assert_eq!(err.0, "sync: version-mismatch");
        assert!(
            configure(&store, "synthetic-peer")
                .unwrap_err()
                .0
                .contains("owner-role-conflict")
        );
        assert!(is_owner(&store).unwrap());
        off(&store).unwrap();
        let err = packet(
            &store,
            &Request {
                protocol: 1,
                force: None,
            },
        )
        .err()
        .unwrap();
        assert!(err.0.contains("owner-not-configured"));
    }
    #[test]
    fn first_line_filter_never_forwards_unknown_text_or_suffixes() {
        assert_eq!(
            remote_line(b"aswap: sync server requires the configured refresh owner\n"),
            "owner-not-configured"
        );
        assert_eq!(
            remote_line(b"aswap: sync: version-mismatch\r\n"),
            "version-mismatch"
        );
        for bad in [
            "synthetic-private-token alice@example.com",
            "aswap: sync server requires the configured refresh owner alice@example.com",
            "aswap: sync: version-mismatch\x1b[31m",
            "{\"access_token\":\"synthetic-private-token\"}",
            "aswap: sync: version-mismatch\nsynthetic-second-line",
        ] {
            assert_eq!(remote_line(bad.as_bytes()), "redacted-unrecognized");
        }
        assert_eq!(remote_line(&[255, 254]), "redacted-unrecognized");
    }
    #[test]
    fn exchange_timeout_bad_response_and_stderr_drain_are_bounded() {
        let request = Request {
            protocol: 1,
            force: None,
        };
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 10"]);
        let start = Instant::now();
        let error = exchange_command(command, &request, Duration::from_millis(50))
            .err()
            .unwrap();
        assert_eq!(error.0, "sync: timeout");
        assert!(start.elapsed() < Duration::from_secs(2));
        let mut command = Command::new("sh");
        command.args([
            "-c",
            "printf 'not-json'; printf 'synthetic-private-stderr' >&2",
        ]);
        let error = exchange_command(command, &request, Duration::from_secs(2))
            .err()
            .unwrap();
        assert!(error.0.contains("bad-response"));
        let mut command = Command::new("sh");
        command.args([
            "-c",
            "printf 'aswap: sync: version-mismatch\\n' >&2; head -c 100000 /dev/zero >&2; exit 1",
        ]);
        let error = exchange_command(command, &request, Duration::from_secs(2))
            .err()
            .unwrap();
        assert_eq!(error.0, "sync: version-mismatch");
    }
    fn follower_store() -> (tempfile::TempDir, Store) {
        let t = tempfile::tempdir().unwrap();
        let store = Store::open(t.path().join("follower")).unwrap();
        store::json_write(
            &store.root.join("coordination.json"),
            &Settings {
                host: Some("synthetic-owner".into()),
                ..Settings::default()
            },
        )
        .unwrap();
        (t, store)
    }
    fn owner_packet(owner: &Store) -> Packet {
        packet(
            owner,
            &Request {
                protocol: 1,
                force: None,
            },
        )
        .unwrap()
    }
    #[test]
    fn a_slow_exchange_never_holds_the_store_lock_or_starts_twice() {
        let (_o, owner) = seed();
        let (_f, store) = follower_store();
        let reply = owner_packet(&owner);
        let (started_tx, started) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel::<()>();
        let slow = {
            let store = Store::open(store.root.clone()).unwrap();
            std::thread::spawn(move || {
                poll_with(&store, None, false, move |host, _| {
                    assert_eq!(host, "synthetic-owner");
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(reply)
                })
            })
        };
        started.recv().unwrap();
        // Another launch takes the store lock at once while the exchange runs,
        let start = Instant::now();
        drop(store.lock().unwrap());
        assert!(start.elapsed() < Duration::from_secs(1));
        // and does not start a second exchange of its own.
        assert_eq!(
            poll_with(&store, None, false, |_, _| panic!("second exchange")).unwrap(),
            0
        );
        release.send(()).unwrap();
        assert_eq!(slow.join().unwrap().unwrap(), 1);
        let after = settings(&store).unwrap();
        assert!(after.last_sync > 0 && after.failures == 0);
        assert_eq!(store.load().unwrap().accounts.len(), 1);
    }
    #[test]
    fn a_failed_exchange_records_backoff_so_peers_do_not_retry() {
        let (_f, store) = follower_store();
        let error = poll_with(&store, None, false, |_, _| Err(Error("sync: timeout")))
            .err()
            .unwrap();
        assert_eq!(error.0, "sync: timeout");
        let after = settings(&store).unwrap();
        assert_eq!(after.failures, 1);
        assert!(after.last_attempt > 0 && after.last_sync == 0);
        // One failure retries 30s later; never the old 120-300s.
        assert!(!due(&after, now()));
        assert!(!due(&after, now() + 29));
        assert!(due(&after, now() + 31));
        assert_eq!(
            poll_with(&store, None, false, |_, _| panic!("retried inside backoff")).unwrap(),
            0
        );
        // An explicit sync still goes through, and success clears the backoff.
        let (_o, owner) = seed();
        let reply = owner_packet(&owner);
        assert_eq!(poll_with(&store, None, true, |_, _| Ok(reply)).unwrap(), 1);
        let after = settings(&store).unwrap();
        assert_eq!(after.failures, 0);
        assert!(!due(&after, now()));
        assert!(due(&after, now() + 31));
    }
    #[test]
    fn a_follower_watch_installs_a_second_packet_pushed_after_the_first() {
        let (o, owner) = seed();
        let (f, store) = follower_store();
        let profile = owner.profile(Provider::Claude, "one").unwrap();
        let rotate = |token: &str| {
            store::json_write(&profile.join(".credentials.json"),&json!({"claudeAiOauth":{"accessToken":token,"refreshToken":"synthetic-owner-refresh","expiresAt":9999999999999u64}})).unwrap();
        };
        // The owner notices its own rotation from file stamps alone.
        let before = auth_print(&owner);
        rotate("synthetic-first-access");
        assert!(auth_print(&owner) != before);
        let first = o.path().join("first.json");
        std::fs::write(
            &first,
            [
                serde_json::to_vec(&owner_packet(&owner)).unwrap(),
                b"\n".to_vec(),
            ]
            .concat(),
        )
        .unwrap();
        rotate("synthetic-second-access");
        let second = o.path().join("second.json");
        std::fs::write(
            &second,
            [
                serde_json::to_vec(&owner_packet(&owner)).unwrap(),
                b"\n".to_vec(),
            ]
            .concat(),
        )
        .unwrap();
        let installed = store
            .profile(Provider::Claude, "one")
            .unwrap()
            .join(".credentials.json");
        let marker = f.path().join("request.json");
        // A fake owner: read the request line, push the first packet, wait
        // until the follower installed it, then push the rotation on the
        // same open stream.
        let script = format!(
            "head -n 1 > '{m}'; cat '{a}'; i=0; until grep -q synthetic-first-access '{p}' 2>/dev/null; do i=$((i+1)); [ $i -gt 200 ] && exit 3; sleep 0.02; done; cat '{b}'",
            m = marker.display(),
            a = first.display(),
            b = second.display(),
            p = installed.display(),
        );
        let start = Instant::now();
        let accepted = watch_session(
            &store,
            |host| {
                assert_eq!(host, "synthetic-owner");
                let mut command = Command::new("sh");
                command.args(["-c", &script]);
                Ok(command)
            },
            Duration::from_secs(5),
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(accepted, Some(2));
        assert!(start.elapsed() < Duration::from_secs(3));
        let request: Value = serde_json::from_slice(&std::fs::read(&marker).unwrap()).unwrap();
        assert_eq!(request["protocol"], WATCH_PROTOCOL);
        let auth: Value = store::json_read(&installed).unwrap();
        assert_eq!(
            auth["claudeAiOauth"]["accessToken"],
            "synthetic-second-access"
        );
        assert!(auth["claudeAiOauth"].get("refreshToken").is_none());
        // A stream that delivered and then ended is not a failure.
        let after = settings(&store).unwrap();
        assert_eq!(after.failures, 0);
        assert!(after.last_sync >= after.last_attempt);
        // Only one process per store streams.
        let held = store.watch_lock().unwrap().unwrap();
        assert!(store.watch_lock().unwrap().is_none());
        watch(&store).unwrap();
        drop(held);
    }
    #[test]
    fn a_failed_or_hanging_watch_backs_off_itself_but_never_the_poll() {
        let (_f, store) = follower_store();
        let poll_state = |s: &Settings| (s.last_sync, s.last_attempt, s.failures);
        let before = settings(&store).unwrap();
        let fail = |script: &'static str, first: Duration| {
            watch_session(
                &store,
                move |_| {
                    let mut command = Command::new("sh");
                    command.args(["-c", script]);
                    Ok(command)
                },
                first,
                Duration::from_secs(5),
            )
            .unwrap_err()
        };
        // An owner that refuses the stream.
        let error = fail(
            "printf 'aswap: sync: version-mismatch\\n' >&2; exit 1",
            Duration::from_secs(5),
        );
        assert_eq!(error.0, "sync: version-mismatch");
        let after = settings(&store).unwrap();
        assert_eq!(after.watch_failures, 1);
        assert!(after.watch_next >= now() + 110);
        assert!(poll_state(&after) == poll_state(&before));
        for at in [now(), now() + 31, now() + 300] {
            assert_eq!(due(&after, at), due(&before, at));
        }
        assert_eq!(
            watch_session(
                &store,
                |_| panic!("reconnected inside backoff"),
                Duration::from_secs(5),
                Duration::from_secs(5)
            )
            .unwrap(),
            None
        );
        // An older owner that waits for stdin EOF and never sends a packet:
        // give up quickly and leave it alone for ten minutes.
        let mut s = settings(&store).unwrap();
        s.watch_next = 0;
        store::json_write(&store.root.join("coordination.json"), &s).unwrap();
        let start = Instant::now();
        let error = fail("cat > /dev/null", Duration::from_millis(300));
        assert_eq!(error.0, NO_STREAM);
        assert!(start.elapsed() < Duration::from_secs(3));
        let after = settings(&store).unwrap();
        assert!(after.watch_next >= now() + 590);
        assert!(poll_state(&after) == poll_state(&before));
        for at in [now(), now() + 31, now() + 300] {
            assert_eq!(due(&after, at), due(&before, at));
        }
        // The poll fallback still runs on its own schedule.
        let (_o, owner) = seed();
        let reply = owner_packet(&owner);
        assert_eq!(poll_with(&store, None, false, |_, _| Ok(reply)).unwrap(), 1);
    }
    #[test]
    fn an_unchanged_heartbeat_leaves_installed_files_alone() {
        let (_o, owner) = seed();
        let (_f, store) = follower_store();
        let profile = store.profile(Provider::Claude, "one").unwrap();
        let first = owner_packet(&owner);
        let issued = first.issued_at;
        {
            let _lock = store.lock().unwrap();
            accept(&store, first).unwrap();
        }
        let auth = crate::shared::stamp(&profile.join(".credentials.json"));
        let config = crate::shared::stamp(&profile.join(".claude.json"));
        let state = crate::shared::stamp(&store.root.join("state.json"));
        assert!(auth.is_some());
        let mut heartbeat = owner_packet(&owner);
        heartbeat.issued_at += 20;
        {
            let _lock = store.lock().unwrap();
            accept(&store, heartbeat).unwrap();
        }
        assert!(crate::shared::stamp(&profile.join(".credentials.json")) == auth);
        assert!(crate::shared::stamp(&profile.join(".claude.json")) == config);
        assert!(crate::shared::stamp(&store.root.join("state.json")) == state);
        let after = settings(&store).unwrap();
        assert_eq!(after.last_issued, issued + 20);
        assert!(after.last_sync > 0);
        // A changed token is still written.
        store::json_write(&owner.profile(Provider::Claude, "one").unwrap().join(".credentials.json"),&json!({"claudeAiOauth":{"accessToken":"synthetic-rotated","refreshToken":"synthetic-owner-refresh","expiresAt":9999999999999u64}})).unwrap();
        install(&store, owner_packet(&owner), &mut settings(&store).unwrap()).unwrap();
        assert!(crate::shared::stamp(&profile.join(".credentials.json")) != auth);
    }
    #[test]
    fn a_follower_never_refreshes_even_for_its_own_running_session() {
        let (_o, owner) = seed();
        let (_f, store) = follower_store();
        install(&store, owner_packet(&owner), &mut settings(&store).unwrap()).unwrap();
        let a = Account {
            provider: Provider::Claude,
            label: "one".into(),
        };
        // Expired, forced and as an active run: still no refresh request.
        for active in [false, true] {
            let _lock = store.lock().unwrap();
            assert!(
                !crate::refresh::refresh_inner(&store, &a, u64::MAX / 2, true, active, |_, _| {
                    panic!("a follower must never rotate")
                })
                .unwrap()
            );
        }
        let auth: Value = store::json_read(
            &store
                .profile(Provider::Claude, "one")
                .unwrap()
                .join(".credentials.json"),
        )
        .unwrap();
        assert!(auth["claudeAiOauth"].get("refreshToken").is_none());
        // Its launch views carry only the public owner-managed sentinel.
        let launch = crate::refresh::launch_auth(Provider::Claude, &auth);
        assert_eq!(
            launch["claudeAiOauth"]["refreshToken"],
            "aswap-owner-managed"
        );
    }

    /// coordination.json as an older release writes it: no watch_* fields,
    /// and its own failure count after repeated failures.
    fn old_writer(store: &Store, last_sync: u64, last_attempt: u64, failures: u32) {
        store::json_write(
            &store.root.join("coordination.json"),
            &json!({"owner":false,"host":"synthetic-owner","last_sync":last_sync,
                "last_attempt":last_attempt,"failures":failures,"last_issued":0}),
        )
        .unwrap();
    }
    #[test]
    fn the_poll_backoff_is_capped_at_a_minute_whatever_older_releases_wrote() {
        let at = now();
        let s = |last_sync, last_attempt, failures| Settings {
            host: Some("synthetic-owner".into()),
            last_sync,
            last_attempt,
            failures,
            ..Default::default()
        };
        // Delivered 30s ago: due.
        assert!(due(&s(at - 30, at - 30, 0), at));
        assert!(!due(&s(at - 29, at - 29, 0), at));
        // In flight (claimed, unfinished): someone else retries after 60s.
        assert!(!due(&s(at - 600, at - 59, 0), at));
        assert!(due(&s(at - 600, at - 60, 0), at));
        // Failed once: 30s; failed many times (an old writer's count): 60s.
        assert!(due(&s(at - 600, at - 30, 1), at));
        assert!(!due(&s(at - 600, at - 59, 9), at));
        assert!(due(&s(at - 600, at - 60, u32::MAX), at));
        // A clock stepped back never postpones the poll.
        assert!(due(&s(at + 3600, at + 3600, 0), at));
        assert!(due(&s(0, at + 3600, 2), at));
        // An old writer dropped the watch fields: still readable, same rule.
        let (_f, store) = follower_store();
        old_writer(&store, at - 600, at - 61, 9);
        let read = settings(&store).unwrap();
        assert_eq!((read.watch_next, read.watch_failures), (0, 0));
        assert!(due(&read, at));
    }
    #[test]
    fn the_fallback_poller_polls_with_no_stream_and_an_old_writers_backoff() {
        let (_o, owner) = seed();
        let (_f, store) = follower_store();
        // The stream is down and an old release recorded nine failures; its
        // own rule would wait 300s. Last delivery and attempt were 61s ago.
        old_writer(&store, now() - 120, now() - 61, 9);
        let reply = std::cell::RefCell::new(Some(owner_packet(&owner)));
        let mut calls = 0;
        poll_loop(&store, Duration::from_millis(10), || {
            calls += 1;
            let result = poll_with(&store, None, false, |_, _| {
                Ok(reply.borrow_mut().take().expect("one exchange"))
            });
            if calls == 3 {
                // Stop the loop: it runs only while this store is a follower.
                off(&store).unwrap();
            }
            result
        });
        assert_eq!(calls, 3);
        // The first tick polled and installed; the next ones were not due.
        assert!(reply.borrow().is_none());
        assert_eq!(store.load().unwrap().accounts.len(), 1);
        let events = recent_events(&store, 10);
        let names: Vec<_> = events
            .iter()
            .map(|e| (e["kind"].as_str().unwrap(), e["event"].as_str().unwrap()))
            .collect();
        assert_eq!(names, [("poll", "claim"), ("poll", "installed")]);
    }
    #[test]
    fn a_stale_packet_counts_as_contact_but_a_clock_step_back_still_installs() {
        let (_o, owner) = seed();
        let (_f, store) = follower_store();
        let first = owner_packet(&owner);
        let issued = first.issued_at;
        {
            let _lock = store.lock().unwrap();
            accept(&store, first).unwrap();
        }
        let profile = store.profile(Provider::Claude, "one").unwrap();
        let rotate = |token: &str| {
            store::json_write(&owner.profile(Provider::Claude, "one").unwrap().join(".credentials.json"),&json!({"claudeAiOauth":{"accessToken":token,"refreshToken":"synthetic-owner-refresh","expiresAt":9999999999999u64}})).unwrap();
        };
        // A slower overlapping exchange: skipped, yet the poll is not left
        // looking in flight (which would wait out the in-flight backoff).
        rotate("synthetic-overlap");
        let mut slow = owner_packet(&owner);
        slow.issued_at = issued - 10;
        let mut s = settings(&store).unwrap();
        s.last_sync = 0;
        s.last_attempt = now();
        store::json_write(&store.root.join("coordination.json"), &s).unwrap();
        let done = {
            let _lock = store.lock().unwrap();
            accept(&store, slow).unwrap()
        };
        assert_eq!(done.event, "stale");
        let after = settings(&store).unwrap();
        assert!(after.last_sync >= after.last_attempt);
        assert_eq!(after.last_issued, issued);
        let auth: Value = store::json_read(&profile.join(".credentials.json")).unwrap();
        assert_eq!(
            auth["claudeAiOauth"]["accessToken"],
            "synthetic-owner-access"
        );
        // The owner's clock stepped back by an hour: install, don't skip
        // every packet until its clock catches up.
        rotate("synthetic-after-clock-step");
        let mut stepped = owner_packet(&owner);
        stepped.issued_at = issued - 3600;
        let done = {
            let _lock = store.lock().unwrap();
            accept(&store, stepped).unwrap()
        };
        assert_eq!(done.event, "installed");
        let auth: Value = store::json_read(&profile.join(".credentials.json")).unwrap();
        assert_eq!(
            auth["claudeAiOauth"]["accessToken"],
            "synthetic-after-clock-step"
        );
    }
    #[test]
    fn the_sync_log_is_private_secret_free_capped_and_shown_in_status() {
        use std::os::unix::fs::PermissionsExt;
        let (_o, owner) = seed();
        let (_f, store) = follower_store();
        poll_with(&store, None, true, |_, _| {
            Err(Error("sync: timeout; synthetic detail"))
        })
        .unwrap_err();
        let reply = owner_packet(&owner);
        poll_with(&store, None, true, |_, _| Ok(reply)).unwrap();
        // A heartbeat that changes nothing is not logged by the stream, and
        // a poll reports it as unchanged.
        let reply = owner_packet(&owner);
        poll_with(&store, None, true, |_, _| Ok(reply)).unwrap();
        let path = store.root.join(LOG);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let text = std::fs::read_to_string(&path).unwrap();
        for secret in [
            "synthetic-owner-access",
            "synthetic-owner-refresh",
            "synthetic-owner\"",
            "synthetic detail",
            "accessToken",
        ] {
            assert!(!text.contains(secret), "{secret} leaked into the sync log");
        }
        let events = recent_events(&store, 10);
        let names: Vec<_> = events
            .iter()
            .map(|e| (e["event"].as_str().unwrap(), e["code"].as_str()))
            .collect();
        assert_eq!(
            names,
            [
                ("claim", None),
                ("fail", Some("timeout")),
                ("claim", None),
                ("installed", None),
                ("claim", None),
                ("unchanged", None),
            ]
        );
        let fail = &events[1];
        assert_eq!(fail["failures"], 1);
        assert_eq!(fail["pid"], std::process::id());
        assert!(fail["next_poll"].as_u64().unwrap() >= fail["last_attempt"].as_u64().unwrap() + 30);
        let status = status(&store).unwrap();
        assert_eq!(status["events"].as_array().unwrap().len(), 6);
        assert!(status["next_poll"].as_u64().is_some());
        // Rotated at the size cap; the previous file is kept once.
        let filler = json!({"event":"x".repeat(1000)});
        for _ in 0..1100 {
            append_log(&path, &filler).unwrap();
        }
        assert!(std::fs::metadata(&path).unwrap().len() < LOG_LIMIT + 2048);
        assert!(store.root.join("sync.log.1").exists());
        // Stream events are logged with their own kind.
        let mut s = settings(&store).unwrap();
        s.watch_next = 0;
        store::json_write(&store.root.join("coordination.json"), &s).unwrap();
        watch_session(
            &store,
            |_| {
                let mut command = Command::new("sh");
                command.args([
                    "-c",
                    "printf 'aswap: sync: version-mismatch\\n' >&2; exit 1",
                ]);
                Ok(command)
            },
            Duration::from_secs(5),
            Duration::from_secs(5),
        )
        .unwrap_err();
        let events = recent_events(&store, 2);
        assert_eq!(events[0]["kind"], "stream");
        assert_eq!(events[0]["event"], "connect");
        assert_eq!(events[1]["event"], "fail");
        assert_eq!(events[1]["code"], "version-mismatch");
        assert!(events[1]["watch_next"].as_u64().unwrap() > now());
    }
}
