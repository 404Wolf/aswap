//! Tells a restarted provider's redrawn login errors from live ones.
use crate::terminal::Terminal;
use std::time::{Duration, Instant};

/// A held login error read this long after a restarted provider records its
/// resume prompt is live. Its redraw is written before the prompt and read
/// at once; a live failure needs an API call and a refresh attempt after.
pub(super) const RESUME_LIVE: Duration = Duration::from_millis(50);
/// Stop holding login errors this long after launch if the transcript never
/// changes; what was held by then is taken for the redraw.
pub(super) const RESUME_QUIET: Duration = Duration::from_secs(10);
pub(super) const QUIET_STEP: Duration = Duration::from_millis(25);
/// Tells a restarted provider's redrawn login errors from live ones by when
/// they were read relative to the first change of its transcript since launch,
/// which is the resume prompt being recorded.
pub(super) struct Quiet {
    since: Instant,
    since_wall: std::time::SystemTime,
    transcript: Option<std::path::PathBuf>,
    launch: Option<crate::shared::Stamp>,
    recorded: Option<Instant>,
}
impl Quiet {
    pub(super) fn new(tracker: Option<&crate::session::Tracker>) -> Self {
        let transcript = tracker.and_then(|t| t.transcript());
        Self {
            since: Instant::now(),
            since_wall: std::time::SystemTime::now(),
            launch: transcript.as_deref().and_then(crate::shared::stamp),
            transcript,
            recorded: None,
        }
    }
    /// One stat; the path comes from the tracker's last walk, or a new one
    /// when `fresh`.
    pub(super) fn check(&mut self, tracker: Option<&crate::session::Tracker>, fresh: bool) {
        if self.recorded.is_some() {
            return;
        }
        if self.transcript.is_none() {
            // Found only now (the tracker learns files once a tick). One
            // unchanged since before launch is the baseline, so finding it is
            // not a change. One written since launch is the resume prompt
            // being recorded: date that by its mtime, since finding it lags.
            self.transcript = tracker.and_then(|t| {
                if fresh {
                    t.transcript_now()
                } else {
                    t.transcript()
                }
            });
            let Some(path) = &self.transcript else {
                return;
            };
            self.launch = crate::shared::stamp(path);
            if let Ok(modified) = std::fs::metadata(path).and_then(|m| m.modified())
                && modified >= self.since_wall
            {
                let ago = std::time::SystemTime::now()
                    .duration_since(modified)
                    .unwrap_or_default();
                self.recorded = Some(Instant::now().checked_sub(ago).unwrap_or_else(Instant::now));
            }
            return;
        }
        if let Some(path) = &self.transcript
            && crate::shared::stamp(path) != self.launch
        {
            self.recorded = Some(Instant::now());
        }
    }
    /// Count held login errors that are live, and stop holding once nothing
    /// read from now on can be part of the redraw.
    pub(super) fn apply(
        &mut self,
        terminal: &mut Terminal,
        tracker: Option<&crate::session::Tracker>,
        fresh: bool,
    ) {
        if !terminal.quiet_auth {
            return;
        }
        self.check(tracker, fresh);
        if let Some(recorded) = self.recorded {
            let live = recorded + RESUME_LIVE;
            terminal.auth_expired |= terminal.held_auth.iter().any(|at| *at > live);
            if Instant::now() > live {
                terminal.quiet_auth = false;
            }
        } else if self.since.elapsed() >= RESUME_QUIET {
            terminal.quiet_auth = false;
        }
        if !terminal.quiet_auth {
            terminal.held_auth.clear();
        }
    }
}
