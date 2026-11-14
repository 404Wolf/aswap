//! Claude records every usage-limit error in the conversation's transcript as
//! an API error entry (`"isApiErrorMessage": true, "error": "rate_limit"`) at
//! the moment it happens. A resumed or resized Claude redraws old errors on
//! screen but never writes them again, and a limit dialog that the terminal
//! parser can't read still writes its entry. So for Claude, the transcript, not
//! the screen, decides whether this provider has hit a limit.
//!
//! Only lines appended after the provider launched are parsed, only the three
//! fields below are looked at, and nothing read is kept or logged.
use crate::session::Tracker;
use serde_json::Value;
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::PathBuf,
    time::{Duration, Instant, SystemTime},
};

/// How far back a transcript first found after launch is read; entries are
/// still filtered by their timestamps.
const TAIL: u64 = 1024 * 1024;
/// At most this much new transcript is read per check.
const STEP: u64 = 4 * 1024 * 1024;
/// A line longer than this is skipped (Claude's error entries are small).
const LINE: usize = 1024 * 1024;
/// Without a known transcript, a limit on screen counts only when shown this
/// long after launch: a resumed provider's redraw is over by then.
pub const UNCONFIRMED: Duration = Duration::from_secs(10);

pub struct Limits {
    launched: Instant,
    launched_wall: SystemTime,
    /// The conversation's transcript, as the tracker last identified it.
    main: Option<Tail>,
    seen: bool,
    screen_seen: bool,
}

/// What has been read of one transcript.
struct Tail {
    path: PathBuf,
    offset: u64,
    partial: Vec<u8>,
    skipping: bool,
}

impl Limits {
    /// Call just before launching the provider. A transcript already known
    /// then (a restart, or an explicit resume) is read from its current end,
    /// so nothing recorded before this launch, such as the limit that ended
    /// the previous provider a moment ago, counts for this one.
    pub fn new(tracker: Option<&Tracker>) -> Self {
        let main = tracker.and_then(|t| t.transcript()).map(|path| {
            let offset = std::fs::metadata(&path).map_or(0, |m| m.len());
            Tail::at(path, offset, false)
        });
        Self {
            launched: Instant::now(),
            launched_wall: SystemTime::now(),
            main,
            seen: false,
            screen_seen: false,
        }
    }

    /// Whether this provider has hit a usage limit: its transcript gained a
    /// limit entry since launch, or, while no transcript is known, the screen
    /// (`screen`: shown since the last check) showed one `UNCONFIRMED` or
    /// more after launch. `fresh` walks the provider home again, for when the
    /// provider is gone and no further `observe` comes.
    pub fn check(&mut self, tracker: Option<&Tracker>, screen: bool, fresh: bool) -> bool {
        if self.seen {
            return true;
        }
        // Follow the tracker: a /clear or another conversation picked inside
        // the provider moves its writes, limits included, to a new transcript.
        let current = tracker.and_then(|t| {
            if fresh {
                t.transcript_now()
            } else {
                t.transcript()
            }
        });
        if let Some(path) = current
            && self.main.as_ref().is_none_or(|tail| tail.path != path)
        {
            self.main = Some(Tail::found(path, self.launched_wall));
        }
        if let Some(tail) = &mut self.main {
            self.seen = tail.read(self.launched_wall);
            return self.seen;
        }
        self.screen_seen |= screen && self.launched.elapsed() >= UNCONFIRMED;
        self.screen_seen
    }
}

impl Tail {
    fn at(path: PathBuf, offset: u64, skipping: bool) -> Self {
        Self {
            path,
            offset,
            partial: Vec::new(),
            skipping,
        }
    }

    /// A transcript first seen after launch: read from its end if nothing
    /// was written since launch, otherwise from its recent tail (entries are
    /// then told apart by their timestamps), skipping the line cut there.
    fn found(path: PathBuf, launched: SystemTime) -> Self {
        let Ok(meta) = std::fs::metadata(&path) else {
            return Self::at(path, 0, false);
        };
        let written = meta.modified().is_ok_and(|m| m >= launched);
        let offset = if written {
            meta.len().saturating_sub(TAIL)
        } else {
            meta.len()
        };
        Self::at(path, offset, offset > 0)
    }

    /// Read what was appended since the last read; whether it holds a limit
    /// entry stamped at or after `since`.
    fn read(&mut self, since: SystemTime) -> bool {
        let Ok(mut file) = File::open(&self.path) else {
            return false;
        };
        let Ok(len) = file.metadata().map(|m| m.len()) else {
            return false;
        };
        if len < self.offset {
            // Replaced or truncated: read the new file from its start.
            self.offset = 0;
            self.partial.clear();
            self.skipping = false;
        }
        if len == self.offset || file.seek(SeekFrom::Start(self.offset)).is_err() {
            return false;
        }
        let mut chunk = Vec::new();
        let Ok(n) = file.take(STEP).read_to_end(&mut chunk) else {
            return false;
        };
        self.offset += n as u64;
        let mut found = false;
        for byte in chunk {
            if byte != b'\n' {
                if self.partial.len() < LINE {
                    self.partial.push(byte);
                } else {
                    self.skipping = true;
                }
                continue;
            }
            if !self.skipping {
                found |= serde_json::from_slice::<Value>(&self.partial)
                    .is_ok_and(|entry| limit_entry(&entry, since));
            }
            self.partial.clear();
            self.skipping = false;
        }
        found
    }
}

/// A Claude transcript entry recording a usage-limit error at or after `since`.
pub fn limit_entry(entry: &Value, since: SystemTime) -> bool {
    if entry.get("isApiErrorMessage").and_then(Value::as_bool) != Some(true)
        || entry.get("error").and_then(Value::as_str) != Some("rate_limit")
    {
        return false;
    }
    let Some(at) = entry
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
    else {
        return false;
    };
    // Entries carry milliseconds.
    at >= chrono::DateTime::<chrono::Utc>::from(since - Duration::from_millis(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const ID: &str = "11111111-2222-4333-8444-555555555555";

    fn stamp(at: SystemTime) -> String {
        chrono::DateTime::<chrono::Utc>::from(at)
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    }
    fn hours_ago(n: u64) -> SystemTime {
        SystemTime::now() - Duration::from_secs(n * 3600)
    }
    fn entry(at: SystemTime, error: &str) -> String {
        serde_json::json!({
            "type": "assistant",
            "timestamp": stamp(at),
            "isApiErrorMessage": true,
            "error": error,
            "message": {"role": "assistant", "content": [{"type": "text",
                "text": "You've hit your weekly limit \u{b7} resets Nov 20, 3am (America/New_York)"}]},
        })
        .to_string()
    }

    fn setup(old: &str) -> (tempfile::TempDir, PathBuf, Tracker) {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("projects").join("-synthetic");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{ID}.jsonl"));
        std::fs::write(&path, old).unwrap();
        let tracker = Tracker::new(
            crate::provider::Provider::Claude,
            home.path(),
            Some(ID.into()),
        )
        .unwrap();
        (home, path, tracker)
    }

    #[test]
    fn a_redrawn_limit_is_not_live_and_an_appended_one_is() {
        let old = hours_ago(4);
        let (_home, path, tracker) = setup(&(entry(old, "rate_limit") + "\n"));
        let mut limits = Limits::new(Some(&tracker));
        // The screen shows the old notice again: the transcript has nothing new.
        assert!(!limits.check(Some(&tracker), true, false));
        assert!(!limits.check(Some(&tracker), true, false));
        // An unrelated error and an ordinary entry are not limits.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(
            file,
            "{}",
            entry(SystemTime::now(), "authentication_failed")
        )
        .unwrap();
        writeln!(
            file,
            "{{\"type\":\"user\",\"timestamp\":\"{}\"}}",
            stamp(SystemTime::now())
        )
        .unwrap();
        assert!(!limits.check(Some(&tracker), false, false));
        // A live limit, written in two pieces, counts whatever the screen says.
        let line = entry(SystemTime::now(), "rate_limit") + "\n";
        let (a, b) = line.split_at(40);
        file.write_all(a.as_bytes()).unwrap();
        assert!(!limits.check(Some(&tracker), false, false));
        file.write_all(b.as_bytes()).unwrap();
        assert!(limits.check(Some(&tracker), false, false));
        assert!(
            limits.check(Some(&tracker), false, false),
            "a limit stays seen"
        );
    }

    #[test]
    fn the_limit_that_ended_the_previous_provider_does_not_count_for_the_next() {
        // Recorded a moment before this restart, on the account just left.
        let (_home, _path, tracker) = setup(&(entry(SystemTime::now(), "rate_limit") + "\n"));
        let mut limits = Limits::new(Some(&tracker));
        assert!(!limits.check(Some(&tracker), true, false));
        assert!(!limits.check(Some(&tracker), true, true));
    }

    #[test]
    fn a_conversation_change_during_a_launch_is_followed() {
        // Launched on A; the provider then switches conversation (a /clear,
        // or a pick inside it) and the tracker follows it to B.
        let (home, _a, mut tracker) = setup("");
        let mut limits = Limits::new(Some(&tracker));
        assert!(!limits.check(Some(&tracker), false, false));
        let other = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
        let b = home
            .path()
            .join("projects")
            .join("-synthetic")
            .join(format!("{other}.jsonl"));
        std::fs::write(&b, "{\"type\":\"user\"}\n").unwrap();
        tracker = Tracker::new(
            crate::provider::Provider::Claude,
            home.path(),
            Some(other.into()),
        )
        .unwrap();
        assert!(!limits.check(Some(&tracker), true, false));
        std::fs::OpenOptions::new()
            .append(true)
            .open(&b)
            .unwrap()
            .write_all((entry(SystemTime::now(), "rate_limit") + "\n").as_bytes())
            .unwrap();
        assert!(limits.check(Some(&tracker), false, false));
    }

    #[test]
    fn a_transcript_found_after_launch_is_read_by_timestamp() {
        // Launch, then the provider appends an old redraw-free history line and
        // a live limit before the transcript is first looked at.
        let mut limits = Limits::new(None);
        let old = hours_ago(4);
        let (_home, path, tracker) = setup(&(entry(old, "rate_limit") + "\n"));
        assert!(
            !limits.check(Some(&tracker), false, false),
            "old entry only"
        );
        let mut late = Limits::new(None);
        std::thread::sleep(Duration::from_millis(5));
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all((entry(SystemTime::now(), "rate_limit") + "\n").as_bytes())
            .unwrap();
        assert!(late.check(Some(&tracker), false, true));
    }

    #[test]
    fn a_screen_limit_counts_only_without_a_transcript_and_after_the_redraw() {
        let mut limits = Limits::new(None);
        // A redraw right after launch...
        assert!(!limits.check(None, true, false));
        limits.launched = Instant::now().checked_sub(UNCONFIRMED).unwrap();
        // ...is not taken for a limit later on,
        assert!(!limits.check(None, false, false));
        // but a notice shown once the redraw is over is.
        assert!(limits.check(None, true, false));
        assert!(limits.check(None, false, false));
        assert!(!Limits::new(None).check(None, false, false));
    }
}
