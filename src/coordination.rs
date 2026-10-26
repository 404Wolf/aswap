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
            host: None,
            last_sync: 0,
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
                last_sync: 0,
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
fn install(store: &Store, packet: Packet, settings: &mut Settings) -> Result<usize> {
    if packet.protocol != 1 {
        return Err(Error("sync: version-mismatch"));
    }
    if packet.profiles.len() > 128 {
        return Err(Error("sync: bad-response"));
    }
    let mut state = store.load()?;
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
    for p in packet.profiles {
        let path = store.profile(p.account.provider, &p.account.label)?;
        store::private_dir(&path)?;
        store::json_write(
            &path.join(p.account.provider.auth_file()),
            &crate::refresh::access_only(p.account.provider, &p.auth),
        )?;
        if p.account.provider == Provider::Claude {
            store::json_write(
                &path.join(".claude.json"),
                &crate::shared::account_config(&p.config),
            )?;
        }
        store::json_write(&path.join("usage-cache.json"), &p.cache)?;
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
            store::json_write(&path, &cache)?;
        }
    }
    store.save(&state)?;
    // Peer scheduling uses the local clock, never the owner's clock as proof
    // of credential validity or freshness. SSH pulls are serialized locally.
    settings.last_sync = now();
    store::json_write(&store.root.join("coordination.json"), settings)?;
    Ok(count)
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
        "aswap sync --serve",
    ]);
    exchange_command(command, request, Duration::from_secs(90))
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
        let stderr = child.stderr.take().ok_or(Error("sync: bad-response"))?;
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
            // Keep draining so verbose SSH/provider errors cannot deadlock the pipe.
            let _ = std::io::copy(&mut reader, &mut std::io::sink());
        });
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
                if !sent {
                    return Err(Error("sync: request-write-failed"));
                }
                let (ok, bytes) = rx
                    .recv_timeout(Duration::from_secs(1))
                    .safe("sync: bad-response; incomplete SSH output")?;
                if !ok || bytes.len() > 4 * 1024 * 1024 {
                    return Err(Error("sync: bad-response; invalid response size"));
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
/// Caller must not already hold the store lock.
pub fn poll(store: &Store, force: Option<&Account>, immediate: bool) -> Result<usize> {
    let initial = settings(store)?;
    if initial.host.is_none() {
        return Ok(0);
    }
    if !immediate && force.is_none() && now() < initial.last_sync.saturating_add(30) {
        return Ok(0);
    }
    let _lock = store.lock()?;
    let mut settings = settings(store)?;
    if !immediate && force.is_none() && now() < settings.last_sync.saturating_add(30) {
        return Ok(0);
    }
    let host = settings
        .host
        .as_deref()
        .ok_or(Error("refresh-owner host missing"))?;
    let packet = exchange(
        host,
        &Request {
            protocol: 1,
            force: force.cloned(),
        },
    )?;
    let duplicates = duplicate_count(&packet);
    let count = install(store, packet, &mut settings)?;
    if immediate && duplicates > 0 {
        eprintln!(
            "aswap sync: duplicate-login warning: {duplicates} duplicate profile(s); aliases retained"
        );
    }
    Ok(count)
}
/// Hidden SSH-only transport; refuse credential output to a terminal or a local
/// invocation. Normal `sync --json` returns counts/role, never this packet.
pub fn serve(store: &Store) -> Result<()> {
    if std::io::stdout().is_terminal()
        || std::io::stdin().is_terminal()
        || std::env::var_os("SSH_CONNECTION").is_none()
    {
        return Err(Error("sync transport is restricted to an SSH pipe"));
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
    let packet = packet(store, &request).map_err(|e| {
        if e.0.starts_with("sync:") {
            e
        } else {
            Error("sync: owner-store-error")
        }
    })?;
    let mut output = std::io::stdout().lock();
    serde_json::to_writer(&mut output, &packet).safe("cannot send owner packet")?;
    output.flush().safe("cannot send owner packet")
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
            install(&peer, serde_json::from_slice(&encoded).unwrap(), &mut cfg).unwrap(),
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
        assert!(serve(&store).is_err());
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
        assert_eq!(install(&peer, p, &mut config).unwrap(), 2);
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
}
