//! Recoverable label transaction. All readers enter through the store lock.
use crate::{
    Error, Result, Safe,
    provider::Provider,
    store::{self, Account, Store},
};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File},
    path::Path,
};
#[derive(Serialize, Deserialize)]
struct Transaction {
    provider: Provider,
    old: String,
    new: String,
}
fn sync(path: &Path) -> Result<()> {
    File::open(path)
        .and_then(|f| f.sync_all())
        .safe("cannot sync renamed profile")
}
fn move_dir(old: &Path, new: &Path) -> Result<()> {
    store::no_links(old)?;
    store::no_links(new)?;
    if old.exists() {
        if !old.is_dir() {
            return Err(Error("rename source must be a directory"));
        }
        if new.exists() {
            return Err(Error("rename destination already exists"));
        }
        fs::rename(old, new).safe("cannot rename profile directory")?;
        sync(old.parent().ok_or(Error("missing rename parent"))?)?;
    }
    Ok(())
}
/// Called only while holding the store lock. Finish a rename interrupted by exit/crash.
pub(crate) fn recover(store: &Store) -> Result<()> {
    let journal = store.root.join("rename-transaction.json");
    if !journal.exists() {
        return Ok(());
    }
    let tx: Transaction = store::json_read(&journal)?;
    store::label(&tx.old)?;
    store::label(&tx.new)?;
    if tx.old == tx.new {
        return Err(Error("invalid rename transaction"));
    }
    let _old_lease = store.lease_label(&Account {
        provider: tx.provider,
        label: tx.old.clone(),
    })?;
    let _new_lease = store.lease_label(&Account {
        provider: tx.provider,
        label: tx.new.clone(),
    })?;
    finish(store, tx)
}
fn finish(store: &Store, tx: Transaction) -> Result<()> {
    let journal = store.root.join("rename-transaction.json");
    let old = store.profile(tx.provider, &tx.old)?;
    let new = store.profile(tx.provider, &tx.new)?;
    let mut state = store.load()?;
    let old_index = state
        .accounts
        .iter()
        .position(|a| a.provider == tx.provider && a.label == tx.old);
    let new_index = state
        .accounts
        .iter()
        .position(|a| a.provider == tx.provider && a.label == tx.new);
    if old_index.is_some() == new_index.is_some() {
        return Err(Error("inconsistent rename transaction"));
    }
    move_dir(&old, &new)?;
    if !new.is_dir() {
        return Err(Error("renamed profile directory is missing"));
    }
    move_dir(
        &store.sessions(tx.provider, &tx.old)?,
        &store.sessions(tx.provider, &tx.new)?,
    )?;
    if let Some(index) = old_index {
        state.accounts[index].label.clone_from(&tx.new);
        if state.active.get(&tx.provider) == Some(&tx.old) {
            state.active.insert(tx.provider, tx.new);
        }
        // Auto policy is provider-wide; preserve its threshold, cooldown and prompt.
        // Usage cache and provider-native histories live inside the moved profile.
        store.save(&state)?;
    }
    fs::remove_file(journal).safe("cannot finish rename transaction")?;
    sync(&store.root)
}
pub fn rename(store: &Store, provider: Provider, old: &str, new: &str) -> Result<Account> {
    store::label(old)?;
    store::label(new)?;
    let _lock = store.lock()?;
    let state = store.load()?;
    let account = store.account(&state, provider, Some(old))?;
    let _old_lease = store.lease_label(&account)?;
    if state
        .accounts
        .iter()
        .any(|a| a.provider == provider && a.label == new)
    {
        return Err(Error("account already exists"));
    }
    let _new_lease = store.lease_label(&Account {
        provider,
        label: new.into(),
    })?;
    let source = store.profile(provider, old)?;
    store::no_links(&source)?;
    if !source.is_dir() {
        return Err(Error("profile directory is missing"));
    }
    for path in [
        store.profile(provider, new)?,
        store.sessions(provider, new)?,
    ] {
        store::no_links(&path)?;
        if path.exists() {
            return Err(Error("rename destination already exists"));
        }
    }
    let sessions = store.sessions(provider, old)?;
    store::no_links(&sessions)?;
    if sessions.exists() && !sessions.is_dir() {
        return Err(Error("session history must be a directory"));
    }
    let tx = Transaction {
        provider,
        old: old.into(),
        new: new.into(),
    };
    store::json_write(&store.root.join("rename-transaction.json"), &tx)?;
    finish(store, tx)?;
    Ok(Account {
        provider,
        label: new.into(),
    })
}
