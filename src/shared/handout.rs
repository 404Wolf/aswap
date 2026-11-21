//! Hand a rotated token to every live view of an account, under each view's
//! own lock.
use super::MCP_OAUTH;
use crate::{
    Error, Result, Safe,
    store::{self, Account, Store},
};
use serde_json::Value;
use std::{
    ffi::OsStr,
    fs::{self, File},
    io::{Read, Write},
    os::unix::fs::{FileExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

/// A launch view's own lock file, beside the view and never inside the
/// provider home. It holds the label of the account whose credentials the view
/// carries, so a hand-out never gives one account's token to a view that has
/// switched to another. Closing the file releases the lock.
/// The lock file beside a launch view (see `view_lock`).
pub(crate) fn view_lock_path(view: &Path) -> PathBuf {
    let mut name = view.as_os_str().to_owned();
    name.push(".lock");
    PathBuf::from(name)
}
pub(super) fn view_lock(view: &Path, create: bool) -> Result<File> {
    let path = view_lock_path(view);
    store::no_links(&path)?;
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .safe("cannot open launch view lock")?;
    fs2::FileExt::lock_exclusive(&file).safe("cannot lock launch view")?;
    Ok(file)
}
pub(super) fn carry(lock: &File, account: &Account) -> Result<()> {
    lock.set_len(0).safe("cannot record launch view account")?;
    lock.write_all_at(account.label.as_bytes(), 0)
        .safe("cannot record launch view account")
}
pub(super) fn carries(mut lock: &File, account: &Account) -> bool {
    let mut label = String::new();
    lock.read_to_string(&mut label).is_ok() && label == account.label
}
/// Replace a view's auth file by rename, so its provider reads the old file or
/// the new one and never a partial write. Views are disposable, so there is
/// no fsync. The temporary name is one `private` skips: a run syncing its view
/// at the same moment never copies it into the shared home.
pub(super) fn replace_view_auth(path: &Path, bytes: &[u8]) -> Result<()> {
    store::no_links(path)?;
    let parent = path.parent().ok_or(Error("missing launch view"))?;
    let name = path
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or(Error("missing launch view"))?;
    let mut temp = tempfile::Builder::new()
        .prefix(&format!("{name}.handout-"))
        .permissions(fs::Permissions::from_mode(0o600))
        .tempfile_in(parent)
        .safe("cannot create private temporary file")?;
    temp.write_all(bytes).safe("cannot write private file")?;
    temp.persist(path).safe("cannot replace private file")?;
    Ok(())
}
/// Hand a rotated account token to every live run's view in one pass, right
/// after its profile changes. The provider revokes the old access token at
/// once, so this cannot wait for each run's tick, and it takes no store lock:
/// each view is replaced by rename under that view's own lock. A view that
/// switched to another account, or a run from an older release without a
/// view lock, is left to its run's own sync. Returns the views updated.
pub(crate) fn hand_out(store: &Store, account: &Account, auth: &Value) -> usize {
    let fresh = crate::refresh::launch_auth(account.provider, auth);
    let runtimes = store.root.join("runtimes");
    crate::live::views(store, account)
        .into_iter()
        .filter(|view| {
            view.starts_with(&runtimes) && hand_to(view, account, &fresh).is_ok_and(|done| done)
        })
        .count()
}
pub(super) fn hand_to(view: &Path, account: &Account, fresh: &Value) -> Result<bool> {
    let lock = view_lock(view, false)?;
    if !carries(&lock, account) {
        return Ok(false);
    }
    let path = view.join(account.provider.auth_file());
    let seen: Value = store::json_read(&path)?;
    let mut next = fresh.clone();
    if account.provider.spec().mcp_oauth()
        && let Some(next) = next.as_object_mut()
    {
        // MCP OAuth in the view is the user's; the run's own sync merges it.
        match seen.get(MCP_OAUTH) {
            Some(mcp) => next.insert(MCP_OAUTH.into(), mcp.clone()),
            None => next.remove(MCP_OAUTH),
        };
    }
    if seen == next {
        return Ok(false);
    }
    replace_view_auth(
        &path,
        &serde_json::to_vec(&next).safe("cannot encode launch auth")?,
    )?;
    Ok(true)
}
