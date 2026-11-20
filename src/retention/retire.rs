use super::{Pressure, files::Dir};
use crate::{Error, Result, Safe};
use rustix::fs::{AtFlags, RenameFlags, renameat_with, unlinkat};
use std::{
    ffi::{OsStr, OsString},
    fs,
    path::Path,
};

pub(super) const PREFIX: &str = ".retiring-";

fn rename(parent: &Dir, source: &OsStr, target: &OsStr, message: &'static str) -> Result<()> {
    renameat_with(&parent.0, source, &parent.0, target, RenameFlags::NOREPLACE).safe(message)
}

pub(super) fn restore(parent: &Dir, retiring: &OsStr, run: &OsStr) -> Result<()> {
    rename(
        parent,
        retiring,
        run,
        "cannot restore retiring view to its original path",
    )?;
    parent.0.sync_all().safe("cannot sync restored view name")
}

pub(super) fn remove(
    parent: &Dir,
    view: &Path,
    verify: impl FnOnce(&Path, &mut Pressure<'_>) -> Result<()>,
    pressure: &mut Pressure<'_>,
    reclaimed: &mut i128,
) -> Result<()> {
    let run = view.file_name().ok_or(Error("view name missing"))?;
    let mut retiring = OsString::from(PREFIX);
    retiring.push(run);
    rename(parent, run, &retiring, "cannot retire view")?;
    let attempt = (|| {
        parent.0.sync_all().safe("cannot sync retiring view name")?;
        verify(&view.with_file_name(&retiring), pressure)?;
        pressure.check(false)?;
        remove_tree(parent, &retiring, pressure, reclaimed, &mut 0)
    })();
    if let Err(error) = attempt {
        restore(parent, &retiring, run)?;
        return Err(error);
    }
    parent.0.sync_all().safe("cannot sync retired view removal")
}

fn remove_tree(
    parent: &Dir,
    name: &OsStr,
    pressure: &mut Pressure<'_>,
    reclaimed: &mut i128,
    entries: &mut usize,
) -> Result<()> {
    let dir = parent
        .contained_child(name)
        .safe("cannot enter retiring view without crossing a mount")?;
    let mut names = dir.names().safe("cannot list retiring view")?;
    // Keep the recorded home available to a retry after partial removal.
    names.sort_by_key(|name| name == crate::shared::VIEW_LIFETIME_LOCK);
    for entry in names {
        *entries += 1;
        pressure.check((*entries).is_multiple_of(64))?;
        let metadata =
            fs::symlink_metadata(dir.path().join(&entry)).safe("cannot inspect retired entry")?;
        if metadata.is_dir() {
            remove_tree(&dir, &entry, pressure, reclaimed, entries)?;
        } else {
            unlinkat(&dir.0, &entry, AtFlags::empty()).safe("cannot remove retired entry")?;
            if metadata.is_file() {
                *reclaimed += i128::from(metadata.len());
            }
        }
    }
    unlinkat(&parent.0, name, AtFlags::REMOVEDIR).safe("cannot remove retired directory")
}
