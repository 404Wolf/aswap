//! Provider indexes that name transcript files. A launch view links its
//! history directories to the shared home, so a provider started there may
//! record a transcript through the view. The view is removed after its run,
//! so such an entry must be rewritten to the home's path, or it dangles.
use crate::session::SessionId;
use std::path::{Path, PathBuf};

/// Which index entries a repair may rewrite.
#[derive(Clone, Copy, Debug)]
pub enum Scope<'a> {
    /// Entries through this one launch view, just before it is removed.
    View(&'a Path),
    /// Entries through any launch view that no longer exists (a run that
    /// ended without cleaning up, or an older aswap).
    Gone,
}

/// What is wrong with an index entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Problem {
    /// The named file is missing.
    Dangling,
    /// The named file exists, but only through a launch view.
    InView,
}

/// One index entry that names a transcript through a launch view, or one
/// that is missing.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Finding {
    /// The provider's database that holds the entry.
    pub database: PathBuf,
    pub id: SessionId,
    pub path: PathBuf,
    pub problem: Problem,
    /// The same transcript in the provider's home, when it exists there and
    /// its name carries the entry's ID: what a repair would write.
    pub canonical: Option<PathBuf>,
}
