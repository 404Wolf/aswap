//! Store patience: how long a run waits out store trouble, and how it reports it.
use crate::{Result, store};
use std::time::Duration;

/// How long store I/O retries while no provider runs (preparing a launch or a
/// restart), and while one does (then its bookkeeping retries next tick).
pub(super) const PREPARE_PATIENCE: Duration = Duration::from_secs(600);
pub(super) const SUPERVISE_PATIENCE: Duration = Duration::from_secs(1);
/// How long store I/O at commit is retried before returning to quota waiting.
pub(super) const COMMIT_PATIENCE: Duration = Duration::from_secs(60);
/// Retry a step that takes and releases the store lock itself, for as long
/// as no provider runs: under the lock, store I/O gives up within seconds so
/// other sessions are never held up, and the waiting happens here, between
/// attempts, with the lock released.
pub(super) fn patiently<T>(what: &str, op: impl FnMut() -> Result<T>) -> Result<T> {
    store::retry_for(what, PREPARE_PATIENCE, Duration::from_millis(200), op)
}
/// Store trouble met while supervising a provider: reported once per kind
/// per launch, never fatal to the provider.
pub(super) struct Hiccups {
    seen: std::collections::BTreeSet<&'static str>,
    /// Whether the provider is still running (it only affects the report).
    pub(super) running: bool,
}
impl Default for Hiccups {
    fn default() -> Self {
        Self {
            seen: Default::default(),
            running: true,
        }
    }
}
impl Hiccups {
    pub(super) fn note<T>(&mut self, what: &'static str, result: Result<T>) -> Option<T> {
        match result {
            Ok(value) => Some(value),
            Err(err) => {
                if self.seen.insert(what) {
                    let then = if self.running {
                        "the provider keeps running"
                    } else {
                        "carrying on without it"
                    };
                    eprintln!("aswap: {what} failed ({err}); {then}");
                }
                None
            }
        }
    }
}
