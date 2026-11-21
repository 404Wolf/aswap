//! SQLite databases that a launch view shares with its home. A view links
//! each database to the home's file, and SQLite then keeps the -wal and
//! -shm beside the real file (it resolves a linked database's path), so all
//! connections share one WAL. A launch view is at risk when that sharing
//! breaks:
//! - its database is a private copy beside the home's: its writes diverge
//!   and are lost with the view;
//! - a sidecar is a private file beside a linked database: a client that
//!   opened the view's spelling has a WAL of its own for a shared database,
//!   which can corrupt it.
//!
//! A sidecar that is simply not linked is safe with SQLite's own
//! resolution. It is listed so that older views can be told apart.
use super::{
    WAL_SIDECARS, entries,
    files::{link, sqlite_database},
    views_root,
};
use crate::{Result, provider::Provider, store::Store};
use std::{ffi::OsString, path::PathBuf};

/// How a launch view's SQLite sharing is broken.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SqliteRisk {
    /// The view holds its own copy of a database that the home has.
    PrivateCopy,
    /// A -wal or -shm beside a linked database is a private file.
    PrivateSidecar,
    /// A private database (recreated in the view) whose -wal or -shm still
    /// links to the home's: its frames would land in the home database's WAL.
    PrivateCopySharedWal,
    /// A -wal or -shm beside a linked database is not linked. Safe while
    /// clients resolve the database link, as SQLite does.
    UnlinkedSidecar,
}
impl SqliteRisk {
    /// Whether this can lose or corrupt data.
    pub fn dangerous(self) -> bool {
        !matches!(self, Self::UnlinkedSidecar)
    }
}

/// One file in one launch view whose SQLite sharing is broken.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ViewRisk {
    pub view: PathBuf,
    pub file: PathBuf,
    pub risk: SqliteRisk,
}

/// Every launch view of `provider` whose SQLite sharing with `home` is
/// broken. Reads only.
pub fn sqlite_risks(
    store: &Store,
    provider: Provider,
    home: &std::path::Path,
) -> Result<Vec<ViewRisk>> {
    let databases: Vec<OsString> = entries(home)?
        .into_iter()
        .filter_map(|path| path.file_name().map(ToOwned::to_owned))
        .filter(|name| sqlite_database(home, name))
        .collect();
    let mut risks = Vec::new();
    if databases.is_empty() {
        return Ok(risks);
    }
    for label in entries(&views_root(store, provider))? {
        for view in entries(&label)? {
            let is_view = view
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("run-"))
                && std::fs::symlink_metadata(&view).is_ok_and(|m| m.is_dir());
            if !is_view {
                continue;
            }
            for database in &databases {
                let file = view.join(database);
                let Ok(meta) = std::fs::symlink_metadata(&file) else {
                    continue;
                };
                if !link(&file) {
                    if meta.is_file() {
                        for suffix in WAL_SIDECARS {
                            let mut name = database.clone();
                            name.push(suffix);
                            let sidecar = view.join(name);
                            if link(&sidecar) {
                                risks.push(ViewRisk {
                                    view: view.clone(),
                                    file: sidecar,
                                    risk: SqliteRisk::PrivateCopySharedWal,
                                });
                            }
                        }
                        risks.push(ViewRisk {
                            view: view.clone(),
                            file,
                            risk: SqliteRisk::PrivateCopy,
                        });
                    }
                    continue;
                }
                for suffix in WAL_SIDECARS {
                    let mut name = database.clone();
                    name.push(suffix);
                    let sidecar = view.join(name);
                    let risk = match std::fs::symlink_metadata(&sidecar) {
                        Err(_) => SqliteRisk::UnlinkedSidecar,
                        Ok(m) if m.file_type().is_symlink() => continue,
                        Ok(_) => SqliteRisk::PrivateSidecar,
                    };
                    risks.push(ViewRisk {
                        view: view.clone(),
                        file: sidecar,
                        risk,
                    });
                }
            }
        }
    }
    Ok(risks)
}
