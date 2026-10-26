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
    io::{IsTerminal, Read, Write},
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
        let _idle = if settings(store)?.host.is_none() {
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
    if request.protocol != 1 || !is_owner(store)? {
        return Err(Error("sync server requires the configured refresh owner"));
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
    if packet.protocol != 1 || packet.profiles.len() > 128 {
        return Err(Error("invalid owner sync packet"));
    }
    let mut state = store.load()?;
    let mut seen = std::collections::BTreeSet::new();
    // Validate the whole packet before writing any account.
    for p in &packet.profiles {
        store::label(&p.account.label)?;
        if !seen.insert((p.account.provider, p.account.label.clone())) {
            return Err(Error("duplicate account in owner packet"));
        }
        provider::validate_auth(
            p.account.provider,
            &serde_json::to_vec(&p.auth).safe("invalid owner authentication")?,
        )?;
        let path = store.profile(p.account.provider, &p.account.label)?;
        if path.join(p.account.provider.auth_file()).exists() {
            let local = crate::identity::read(p.account.provider, &path).unwrap_or_default();
            let remote = crate::identity::from_values(p.account.provider, &p.auth, &p.config);
            if local.known() && remote.known() && !crate::identity::same(&local, &remote) {
                return Err(Error(
                    "owner profile identity mismatch; align profile labels first",
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
fn exchange(host: &str, request: &Request) -> Result<Packet> {
    if !valid_host(host) {
        return Err(Error("invalid sync host alias"));
    }
    let mut child = Command::new("ssh")
        .args([
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
        ])
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .safe("cannot contact refresh owner over SSH")?;
    let result = (|| {
        let mut input = child.stdin.take().ok_or(Error("missing sync input"))?;
        serde_json::to_writer(&mut input, request).safe("cannot send sync request")?;
        input.write_all(b"\n").safe("cannot send sync request")?;
        drop(input);
        let output = child.stdout.take().ok_or(Error("missing sync output"))?;
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let result = output.take(4 * 1024 * 1024 + 1).read_to_end(&mut bytes);
            let _ = tx.send((result.is_ok(), bytes));
        });
        let deadline = Instant::now() + Duration::from_secs(90);
        while Instant::now() < deadline {
            if let Some(status) = child.try_wait().safe("refresh-owner SSH failed")? {
                if !status.success() {
                    return Err(Error("refresh-owner SSH failed; details redacted"));
                }
                let (ok, bytes) = rx
                    .recv_timeout(Duration::from_secs(1))
                    .safe("refresh-owner response incomplete")?;
                if !ok || bytes.len() > 4 * 1024 * 1024 {
                    return Err(Error("invalid refresh-owner response size"));
                }
                return serde_json::from_slice(&bytes)
                    .safe("invalid refresh-owner protocol; contents redacted");
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        Err(Error("refresh-owner SSH timed out"))
    })();
    if child.try_wait().ok().flatten().is_none() {
        // SAFETY: this still-live child owns the new process group.
        unsafe {
            libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
        }
    }
    let _ = child.wait();
    result
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
    install(store, packet, &mut settings)
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
    let request: Request = serde_json::from_slice(&bytes).safe("invalid sync request")?;
    let packet = packet(store, &request)?;
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
}
