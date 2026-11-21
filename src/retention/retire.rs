use super::{
    Pressure,
    files::{Dir, sqlite_sidecar_sort_base},
};
use crate::{Error, Result, Safe};
use rustix::fs::{AtFlags, RenameFlags, renameat_with, unlinkat};
use std::{
    ffi::{OsStr, OsString},
    fs,
    os::unix::fs::MetadataExt,
    path::Path,
};

pub(super) const PREFIX: &str = ".retiring-";

fn rename(parent: &Dir, source: &OsStr, target: &OsStr, message: &'static str) -> Result<()> {
    renameat_with(&parent.0, source, &parent.0, target, RenameFlags::NOREPLACE).safe(message)
}

fn identity(dir: &Dir) -> Result<(u64, u64)> {
    let metadata = dir
        .0
        .metadata()
        .safe("cannot identify retiring directory")?;
    Ok((metadata.dev(), metadata.ino()))
}

fn require_identity(parent: &Dir, name: &OsStr, dir: &Dir) -> Result<()> {
    let named = parent
        .contained_child(name)
        .safe("cannot reopen retiring directory")?;
    if identity(&named)? != identity(dir)? {
        return Err(Error("retiring directory identity changed"));
    }
    Ok(())
}

pub(super) fn restore(parent: &Dir, retiring: &OsStr, run: &OsStr, dir: &Dir) -> Result<()> {
    require_identity(parent, retiring, dir)?;
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
    inventoried: &Dir,
    verify: impl FnOnce(&Path, &mut Pressure<'_>) -> Result<()>,
    pressure: &mut Pressure<'_>,
    reclaimed: &mut i128,
) -> Result<()> {
    let run = view.file_name().ok_or(Error("view name missing"))?;
    let mut retiring = OsString::from(PREFIX);
    retiring.push(run);
    rename(parent, run, &retiring, "cannot retire view")?;
    let dir = parent
        .contained_child(&retiring)
        .safe("cannot open retiring view without crossing a mount")?;
    if identity(&dir)? != identity(inventoried)? {
        return Err(Error("retiring directory differs from inventoried view"));
    }
    let attempt = (|| {
        parent.0.sync_all().safe("cannot sync retiring view name")?;
        verify(&view.with_file_name(&retiring), pressure)?;
        pressure.check(false)?;
        remove_tree(parent, &retiring, &dir, pressure, reclaimed, &mut 0)
    })();
    if let Err(error) = attempt {
        restore(parent, &retiring, run, &dir)?;
        return Err(error);
    }
    parent.0.sync_all().safe("cannot sync retired view removal")
}

fn remove_tree(
    parent: &Dir,
    name: &OsStr,
    dir: &Dir,
    pressure: &mut Pressure<'_>,
    reclaimed: &mut i128,
    entries: &mut usize,
) -> Result<()> {
    require_identity(parent, name, dir)?;
    let mut names = dir.names().safe("cannot list retiring view")?;
    // Remove sidecars before SQLite main files. Keep the home marker until
    // last so an interrupted cleanup still knows the original provider home.
    names.sort_by_cached_key(|name| {
        let group = sqlite_sidecar_sort_base(name);
        (
            name == crate::shared::VIEW_LIFETIME_LOCK,
            group.unwrap_or(name).to_owned(),
            group.is_none(),
        )
    });
    for entry in names {
        *entries += 1;
        pressure.check((*entries).is_multiple_of(64))?;
        let metadata =
            fs::symlink_metadata(dir.path().join(&entry)).safe("cannot inspect retired entry")?;
        if metadata.is_dir() {
            let child = dir
                .contained_child(&entry)
                .safe("cannot enter retiring view without crossing a mount")?;
            remove_tree(dir, &entry, &child, pressure, reclaimed, entries)?;
        } else {
            unlinkat(&dir.0, &entry, AtFlags::empty()).safe("cannot remove retired entry")?;
            if metadata.is_file() {
                *reclaimed += i128::from(metadata.len());
            }
        }
    }
    require_identity(parent, name, dir)?;
    unlinkat(&parent.0, name, AtFlags::REMOVEDIR).safe("cannot remove retired directory")
}
