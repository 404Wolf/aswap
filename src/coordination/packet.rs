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
    /// The providers this follower can install. An older follower sends
    /// none: it gets Claude and Codex profiles only, since a profile of a
    /// provider it doesn't know would fail its whole packet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) providers: Option<Vec<String>>,
}
impl Request {
    pub(super) fn new(protocol: u8, force: Option<Account>) -> Self {
        Self {
            protocol,
            force,
            providers: Some(Provider::ALL.iter().map(|p| p.as_str().into()).collect()),
        }
    }
    /// Whether this follower can install `provider`'s profiles.
    pub(super) fn accepts(&self, provider: Provider) -> bool {
        match &self.providers {
            Some(list) => list.iter().any(|p| p == provider.as_str()),
            None => matches!(provider, Provider::Claude | Provider::Codex),
        }
    }
}
#[derive(Serialize, Deserialize)]
pub(super) struct Profile {
    pub(super) account: Account,
    pub(super) auth: Value,
    pub(super) config: Value,
    #[serde(with = "packet_cache")]
    pub(super) cache: Cached,
}
/// How a cache crosses protocol 1, so older peers still read it safely.
mod packet_cache {
    use crate::usage::{Cached, UnverifiedLimit};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    const UNVERIFIED: &str = "usage-unverified";

    #[derive(Serialize, Deserialize)]
    struct WireCache {
        #[serde(flatten)]
        cache: Cached,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        retained_limits: Vec<UnverifiedLimit>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        named_codex_slots: Vec<Option<u8>>,
    }

    pub(super) fn serialize<S: Serializer>(
        cache: &Cached,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let has_named_slots = cache
            .usage
            .windows
            .iter()
            .any(|window| window.duration_minutes.is_some() && window.codex_slot.is_some());
        if cache.unverified_limits.is_empty() && !has_named_slots {
            return cache.serialize(serializer);
        }
        let mut cache = cache.clone();
        // Protocol 1 readers disagree on the shape of unverified_limits.
        // Older peers can all decode the base cache and block on an error.
        let retained_limits = std::mem::take(&mut cache.unverified_limits);
        if !retained_limits.is_empty() {
            cache.error.get_or_insert_with(|| UNVERIFIED.into());
        }
        // Older readers match named windows with a missing slot. Carry the
        // newly available identities separately in the same protocol.
        let named_codex_slots = if has_named_slots {
            cache
                .usage
                .windows
                .iter_mut()
                .map(|window| {
                    if window.duration_minutes.is_some() {
                        window.codex_slot.take()
                    } else {
                        None
                    }
                })
                .collect()
        } else {
            Vec::new()
        };
        WireCache {
            cache,
            retained_limits,
            named_codex_slots,
        }
        .serialize(serializer)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Cached, D::Error> {
        let WireCache {
            mut cache,
            retained_limits,
            named_codex_slots,
        } = WireCache::deserialize(deserializer)?;
        if !retained_limits.is_empty() {
            if cache.error.as_deref() == Some(UNVERIFIED) {
                cache.error = None;
            }
            cache.unverified_limits.extend(retained_limits);
        }
        for (window, slot) in cache.usage.windows.iter_mut().zip(named_codex_slots) {
            if slot.is_some() {
                window.codex_slot = slot;
            }
        }
        Ok(cache)
    }
}
#[derive(Serialize, Deserialize)]
pub(super) struct Packet {
    pub(super) protocol: u8,
    pub(super) issued_at: u64,
    /// Profiles of providers this version doesn't know are skipped, so a
    /// newer owner never breaks an older follower's whole sync.
    #[serde(deserialize_with = "known_profiles")]
    pub(super) profiles: Vec<Profile>,
}
fn known_profiles<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Vec<Profile>, D::Error> {
    use serde::de::Error as _;
    let raw: Vec<Value> = Vec::deserialize(deserializer)?;
    let mut profiles = Vec::new();
    for profile in raw {
        let provider = profile.pointer("/account/provider").and_then(Value::as_str);
        let known = provider.is_none_or(|name| {
            serde_json::from_value::<crate::provider::Provider>(Value::String(name.to_owned()))
                .is_ok()
        });
        if known {
            profiles.push(serde_json::from_value(profile).map_err(D::Error::custom)?);
        }
    }
    Ok(profiles)
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
    for account in accounts.into_iter().filter(|a| request.accepts(a.provider)) {
        let force = request
            .force
            .as_ref()
            .is_some_and(|a| a.provider == account.provider && a.label == account.label);
        let cache = usage::cached_force(store, &account, force)?;
        let _lock = store.lock()?;
        let path = store.profile(account.provider, &account.label)?;
        let auth: Value = store::json_read(&path.join(account.provider.auth_file()))?;
        let config = match account
            .provider
            .spec()
            .account_config()
            .map(|f| path.join(f))
        {
            Some(file) if file.exists() => crate::shared::account_config(&store::json_read(&file)?),
            _ => json!({}),
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
            // The legacy key is Codex's workspace, which teammates share; the
            // account key (where both logins carry it) tells them apart.
            let other_member = matches!(
                (local.account_key(), remote.account_key()),
                (Some(a), Some(b)) if a != b
            );
            if local.known() && remote.known() && !crate::identity::same(&local, &remote)
                || other_member
            {
                return Err(Error(
                    "sync: identity-conflict; same label has a different account; align labels first",
                ));
            }
        }
    }
    let count = packet.profiles.len();
    let mut changed = false;
    for mut p in packet.profiles {
        let path = store.profile(p.account.provider, &p.account.label)?;
        store::private_dir(&path)?;
        let auth = crate::refresh::access_only(p.account.provider, &p.auth);
        if write_changed(&path.join(p.account.provider.auth_file()), &auth)? {
            // The owner's refresh already revoked the old token here too.
            crate::shared::hand_out(store, &p.account, &auth);
            changed = true;
        }
        if let Some(file) = p.account.provider.spec().account_config() {
            write_changed(&path.join(file), &crate::shared::account_config(&p.config))?;
        }
        let cache_path = path.join("usage-cache.json");
        if let Ok(old) = store::json_read(&cache_path) {
            p.cache.preserve_limits(&old);
        }
        write_changed(&cache_path, &p.cache)?;
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
            *provider == p.account.provider && crate::identity::same_account(prior, &identity)
        }) {
            count += 1;
        } else {
            seen.push((p.account.provider, identity));
        }
    }
    count
}

#[cfg(test)]
mod packet_compat_tests {
    use super::*;

    #[test]
    fn a_packet_profile_of_an_unknown_provider_is_skipped() {
        let known = Profile {
            account: Account {
                provider: crate::provider::Provider::Codex,
                label: "work".into(),
            },
            auth: json!({"tokens":{"access_token":"synthetic"}}),
            config: json!({}),
            cache: Cached::default(),
        };
        let mut value = serde_json::to_value(Packet {
            protocol: 1,
            issued_at: 1,
            profiles: vec![known],
        })
        .unwrap();
        let mut newer = value["profiles"][0].clone();
        newer["account"]["provider"] = "agy".into();
        value["profiles"].as_array_mut().unwrap().insert(0, newer);
        let packet: Packet = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(packet.profiles.len(), 1);
        assert_eq!(packet.profiles[0].account.label, "work");
        // A known provider's broken profile still fails the packet.
        value["profiles"][1]["account"]["label"] = 7.into();
        assert!(serde_json::from_value::<Packet>(value).is_err());
    }
}
