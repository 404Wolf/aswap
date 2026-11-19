//! Tells a restarted provider's redrawn login errors from live ones.
use crate::terminal::Terminal;
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
    time::{Duration, Instant, SystemTime},
};

/// A held login error read this long after a restarted provider records its
/// resume prompt is live. Its redraw is written before the prompt and read
/// at once; a live failure needs an API call and a refresh attempt after.
pub(super) const RESUME_LIVE: Duration = Duration::from_millis(50);
/// Stop holding login errors this long after launch if the transcript never
/// changes; what was held by then is taken for the redraw.
pub(super) const RESUME_QUIET: Duration = Duration::from_secs(10);
pub(super) const QUIET_STEP: Duration = Duration::from_millis(25);
/// Maximum transcript data read once when the path is learned after launch.
pub(super) const QUIET_HISTORY: u64 = 1024 * 1024;

pub(super) fn first_transcript_write(path: &Path, since: SystemTime) -> Option<SystemTime> {
    let mut file = File::open(path).ok()?;
    let metadata = file.metadata().ok()?;
    let modified = metadata.modified().ok().filter(|at| *at >= since)?;
    let created = metadata.created().ok().filter(|at| *at >= since);
    // A new file has the first write at its head. For an older file, look
    // for the first record from this launch in its recent tail.
    let offset = if created.is_some() {
        0
    } else {
        metadata.len().saturating_sub(QUIET_HISTORY)
    };
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut bytes = Vec::new();
    file.take(QUIET_HISTORY).read_to_end(&mut bytes).ok()?;
    let recorded = bytes
        .split_inclusive(|byte| *byte == b'\n')
        .skip(usize::from(offset != 0))
        .find_map(|line| {
            let entry: serde_json::Value =
                serde_json::from_slice(line.strip_suffix(b"\n")?).ok()?;
            let timestamp = entry.get("timestamp")?.as_str()?;
            let at = SystemTime::from(chrono::DateTime::parse_from_rfc3339(timestamp).ok()?);
            (at >= since).then_some(at)
        });
    // A newly created file can also mark its first write on filesystems
    // with birth times. With neither signal, use the observed mtime.
    Some(recorded.or(created).unwrap_or(modified))
}

/// Tells a restarted provider's redrawn login errors from live ones by when
/// they were read relative to the first change of its transcript since launch,
/// which is the resume prompt being recorded.
pub(super) struct Quiet {
    pub(super) since: Instant,
    pub(super) since_wall: SystemTime,
    pub(super) transcript: Option<std::path::PathBuf>,
    pub(super) launch: Option<crate::shared::Stamp>,
    pub(super) recorded: Option<Instant>,
}
impl Quiet {
    pub(super) fn new(tracker: Option<&crate::session::Tracker>) -> Self {
        let transcript = tracker.and_then(|t| t.transcript());
        Self {
            since: Instant::now(),
            since_wall: SystemTime::now(),
            launch: transcript.as_deref().and_then(crate::shared::stamp),
            transcript,
            recorded: None,
        }
    }
    /// Whether `check` can still learn anything: the prompt is not yet
    /// recorded and the transcript is known.
    pub(super) fn needs_check(&self) -> bool {
        self.recorded.is_none() && self.transcript.is_some()
    }
    /// Check the cached transcript's stamp without walking the history.
    pub(super) fn check(&mut self) {
        if self.recorded.is_some() {
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
    ) {
        if !terminal.quiet_auth {
            return;
        }
        if self.transcript.is_none()
            && let Some(path) = tracker.and_then(crate::session::Tracker::transcript)
        {
            // The prompt may have been followed by more writes before this
            // lookup. Date it from the first record of this launch.
            self.launch = crate::shared::stamp(&path);
            self.recorded = first_transcript_write(&path, self.since_wall)
                .and_then(|at| at.duration_since(self.since_wall).ok())
                .map(|elapsed| (self.since + elapsed).min(Instant::now()));
            self.transcript = Some(path);
        }
        self.check();
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

#[cfg(test)]
mod quiet_tests {
    use super::*;
    use crate::{provider::Provider, session::Tracker};
    use std::{
        fs::{self, FileTimes},
        io::Write,
        os::unix::fs::PermissionsExt,
    };

    #[test]
    fn a_redraw_just_after_the_prompt_stays_quiet() {
        let mut quiet = Quiet::new(None);
        let prompt = Instant::now() - 2 * RESUME_LIVE;
        quiet.recorded = Some(prompt);
        let mut terminal = Terminal::open().unwrap();
        terminal.quiet_auth = true;
        terminal.held_auth.push(prompt + Duration::from_millis(20));
        quiet.apply(&mut terminal, None);
        assert!(!terminal.auth_expired);

        terminal.quiet_auth = true;
        terminal
            .held_auth
            .push(prompt + RESUME_LIVE + Duration::from_millis(1));
        quiet.apply(&mut terminal, None);
        assert!(terminal.auth_expired);
    }

    #[test]
    fn chmod_does_not_make_a_redrawn_login_error_live() {
        let home = tempfile::tempdir().unwrap();
        let id = "11111111-2222-4333-8444-555555555555";
        let dir = home.path().join("projects/-synthetic");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{id}.jsonl"));
        fs::write(&path, "{\"type\":\"user\"}\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let tracker = Tracker::new(Provider::Claude, home.path(), Some(id.into())).unwrap();
        let mut quiet = Quiet::new(Some(&tracker));
        let mut terminal = Terminal::open().unwrap();
        terminal.quiet_auth = true;

        std::thread::sleep(Duration::from_millis(5));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        quiet.check();
        std::thread::sleep(RESUME_LIVE + Duration::from_millis(10));
        terminal.held_auth.push(Instant::now());
        quiet.apply(&mut terminal, Some(&tracker));

        assert!(
            !terminal.auth_expired,
            "securing the transcript did not record the prompt"
        );
        assert!(terminal.quiet_auth);
    }

    #[test]
    fn a_login_failure_after_a_late_discovered_prompt_counts_at_exit() {
        let home = tempfile::tempdir().unwrap();
        let id = "11111111-2222-4333-8444-555555555555";
        let mut tracker = Tracker::new(Provider::Claude, home.path(), Some(id.into())).unwrap();
        let mut quiet = Quiet::new(Some(&tracker));
        let mut terminal = Terminal::open().unwrap();
        terminal.quiet_auth = true;
        quiet.since -= Duration::from_secs(2);
        quiet.since_wall -= Duration::from_secs(2);
        let pid = std::process::id();
        tracker.observe(pid).unwrap();
        assert!(tracker.transcript().is_none());

        let dir = home.path().join("projects/-synthetic");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{id}.jsonl"));
        let prompt = Duration::from_millis(500);
        let error = Duration::from_millis(900);
        let since_wall = quiet.since_wall;
        let entry = |kind: &str, at| {
            serde_json::json!({"type": kind,
                "timestamp": chrono::DateTime::<chrono::Utc>::from(since_wall + at)
                    .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)})
        };
        std::fs::write(&path, format!("{}\n", entry("user", prompt))).unwrap();
        tracker.observe(pid).unwrap();
        assert!(tracker.transcript().is_none(), "the retry is still pending");
        quiet.apply(&mut terminal, Some(&tracker));

        let mut file = File::options().append(true).open(&path).unwrap();
        writeln!(file, "{}", entry("assistant", error)).unwrap();
        file.set_times(FileTimes::new().set_modified(quiet.since_wall + error))
            .unwrap();
        drop(file);
        terminal.held_auth.push(quiet.since + prompt);
        terminal
            .held_auth
            .push(quiet.since + error + Duration::from_millis(10));
        tracker.transcript_now();
        quiet.apply(&mut terminal, Some(&tracker));

        assert_eq!(quiet.recorded, Some(quiet.since + prompt));
        assert!(
            terminal.auth_expired,
            "the second error followed the prompt"
        );
    }
}
