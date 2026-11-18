//! The `aswap auto` monitor tick and the usage observations it ranks.
use super::now;
use crate::{
    Result,
    provider::Provider,
    store::{self, Account, Store},
    usage,
};

pub fn observations(
    store: &Store,
    provider: Provider,
) -> Result<(Vec<(Account, usage::Cached)>, store::Auto)> {
    let (accounts, settings) = {
        let _lock = store.lock()?;
        let state = store.load()?;
        (
            state
                .accounts
                .into_iter()
                .filter(|a| a.provider == provider)
                .collect::<Vec<_>>(),
            state.auto.get(&provider).cloned().unwrap_or_default(),
        )
    };
    let entries = accounts
        .into_iter()
        .map(|a| usage::cached(store, &a).map(|c| (a, c)))
        .collect::<Result<Vec<_>>>()?;
    Ok((entries, settings))
}
/// Auto changes the launcher's active account. Existing isolated runs retain
/// their profile; explicit --activate-live swaps the default Claude login.
pub fn tick(store: &Store, provider: Provider, activate_live: bool) -> Result<Option<String>> {
    {
        let _lock = store.lock()?;
        if !store.load()?.auto.get(&provider).is_some_and(|s| s.enabled) {
            return Ok(None);
        }
    }
    let (entries, _) = observations(store, provider)?;
    let _lock = store.lock()?;
    let mut state = store.load()?;
    let settings = state.auto.get(&provider).cloned().unwrap_or_default();
    if !settings.enabled {
        return Ok(None);
    }
    let Some(current) = state.active.get(&provider) else {
        return Ok(None);
    };
    let Some(account) = usage::choose(
        current,
        &entries,
        settings.threshold,
        settings.prefer,
        settings.last_switch,
        now(),
        false,
    )
    .or_else(|| {
        usage::prefer_at_launch(
            current,
            &entries,
            settings.threshold,
            settings.prefer,
            now(),
        )
    }) else {
        return Ok(None);
    };
    if activate_live {
        crate::provider::activate_profile(store, account)?;
    }
    state.active.insert(provider, account.label.clone());
    state.auto.entry(provider).or_default().last_switch = now();
    store.save(&state)?;
    Ok(Some(account.label.clone()))
}

pub(super) fn entries_from_cache(
    store: &Store,
    provider: Provider,
) -> Result<Vec<(Account, usage::Cached)>> {
    store
        .load()?
        .accounts
        .into_iter()
        .filter(|a| a.provider == provider)
        .map(|a| {
            let path = store.profile(provider, &a.label)?.join("usage-cache.json");
            let cache = if path.exists() {
                store::json_read(&path)?
            } else {
                usage::Cached::default()
            };
            Ok((a, cache))
        })
        .collect()
}
