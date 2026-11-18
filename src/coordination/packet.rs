//! Owner packets: what an owner exports and how a follower installs it.
use super::*;

/// Write `value` only when the file does not already hold it: heartbeat
/// packets must leave profile auth stamps alone, or every running view would
/// resync on each heartbeat.
pub(super) fn write_changed<T: Serialize>(path: &std::path::Path, value: &T) -> Result<bool> {
    let value = serde_json::to_value(value).safe("cannot encode private state")?;
    if path.exists() && store::json_read::<Value>(path).ok().as_ref() == Some(&value) {
        return Ok(false);
    }
    store::json_write(path, &value).map(|()| true)
}
// These are transport-only types. Never derive Debug, format packets in errors,
// store them in logs, or return them from an ordinary CLI JSON command.
#[derive(Serialize, Deserialize)]
pub(super) struct Request {
    pub(super) protocol: u8,
    pub(super) force: Option<Account>,
}
#[derive(Serialize, Deserialize)]
pub(super) struct Profile {
    pub(super) account: Account,
    pub(super) auth: Value,
    pub(super) config: Value,
    pub(super) cache: Cached,
}
#[derive(Serialize, Deserialize)]
pub(super) struct Packet {
    pub(super) protocol: u8,
    pub(super) issued_at: u64,
    pub(super) profiles: Vec<Profile>,
}
pub(super) fn packet(store: &Store, request: &Request) -> Result<Packet> {
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
#[cfg(test)]
pub(super) fn install(
    store: &Store,
    packet: Packet,
    settings: &mut Settings,
) -> Result<(usize, bool)> {
    let policy = SyncPolicy::read(&store.sync_policy_path)?;
    policy.check_local(store)?;
    install_permitted(store, packet, settings, &policy)
}
pub(super) fn install_permitted(
    store: &Store,
    mut packet: Packet,
    settings: &mut Settings,
    policy: &SyncPolicy,
) -> Result<(usize, bool)> {
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
    }
    packet.profiles.retain(|p| policy.permits(&p.account));
    // Denied profiles never participate in auth/identity checks or writes.
    for p in &packet.profiles {
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
        let auth = crate::refresh::access_only(p.account.provider, &p.auth);
        if write_changed(&path.join(p.account.provider.auth_file()), &auth)? {
            // The owner's refresh already revoked the old token here too.
            crate::shared::hand_out(store, &p.account, &auth);
            changed = true;
        }
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
#[cfg(test)]
pub(super) fn duplicate_count(packet: &Packet) -> usize {
    duplicate_count_permitted(packet, &SyncPolicy::default())
}
pub(super) fn duplicate_count_permitted(packet: &Packet, policy: &SyncPolicy) -> usize {
    let mut seen: Vec<(Provider, crate::identity::Identity)> = Vec::new();
    let mut count = 0;
    for p in packet
        .profiles
        .iter()
        .filter(|p| policy.permits(&p.account))
    {
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
