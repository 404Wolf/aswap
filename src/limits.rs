//! Claude usually records usage limits in the transcript as API errors.
//! A resumed or resized Claude redraws old errors without recording them
//! again. Its compaction and limit dialog can also block without a new entry.
//!
//! Transcript entries must belong to the current launch. Screen-only
//! compaction and dialog evidence needs a fresh observation of exhausted quota.
use crate::{
    session::Tracker,
    shared::Stamp,
    terminal::{InputActivity, ScreenLimits},
    usage,
};
use serde_json::Value;
use std::{
    fs::{File, Metadata},
    io::{Read, Seek, SeekFrom},
    os::unix::fs::MetadataExt,
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
/// How many bytes before the read position are kept to notice a transcript
/// rewritten in place (truncated and written again, to any length).
const ANCHOR: usize = 256;
/// Without a known transcript, a limit on screen counts only when shown this
/// long after launch: a resumed provider's redraw is over by then.
pub const UNCONFIRMED: Duration = Duration::from_secs(10);

pub struct Limits {
    launched: Instant,
    launched_wall: SystemTime,
    session_id: Option<String>,
    /// The conversation's transcript, as the tracker last identified it.
    main: Option<Tail>,
    /// Its subagents' transcripts written since launch: a subagent can hit a
    /// limit (and record it only there) before the main thread calls again.
    subagents: std::collections::BTreeMap<PathBuf, Tail>,
    scanned: Option<Instant>,
    seen: bool,
    screen_seen: bool,
    /// The tracked provider, which decides what a limit entry looks like.
    spec: Option<&'static dyn crate::provider::Spec>,
    pending: Option<PendingScreen>,
}

struct PendingScreen {
    evidence: ScreenLimits,
    last_input: Option<Instant>,
    transcript: Option<TranscriptVersion>,
    retry: Instant,
}

/// What has been read of one transcript.
struct Tail {
    path: PathBuf,
    /// The version read through its observed end, with no queued bytes.
    read_stamp: Option<TranscriptVersion>,
    offset: u64,
    /// The bytes just before `offset`, as last read.
    anchor: Vec<u8>,
    partial: Vec<u8>,
    skipping: bool,
}

#[derive(PartialEq)]
struct TranscriptVersion(Stamp, i64, i64);

impl TranscriptVersion {
    fn from_metadata(metadata: &Metadata) -> Self {
        // ctime catches a same-length rewrite that restores the old mtime.
        Self(
            Stamp::from_metadata(metadata),
            metadata.ctime(),
            metadata.ctime_nsec(),
        )
    }
}

/// Up to `ANCHOR` bytes of `file` just before `offset`.
fn anchor(file: &mut File, offset: u64) -> Option<Vec<u8>> {
    let start = offset.saturating_sub(ANCHOR as u64);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.take(offset - start).read_to_end(&mut bytes).ok()?;
    Some(bytes)
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
            session_id: tracker.and_then(Tracker::known_id).map(str::to_owned),
            main,
            subagents: Default::default(),
            scanned: None,
            seen: false,
            screen_seen: false,
            spec: tracker.map(|t| t.provider().spec()),
            pending: None,
        }
    }

    /// Whether this provider has hit a usage limit: its transcript gained a
    /// limit entry since launch, or, while no transcript is known, the screen
    /// (`screen`: shown since the last check) showed one `UNCONFIRMED` or
    /// more after launch. `fresh` checks subagents even if they were just
    /// scanned, for when the provider is gone.
    pub fn check(&mut self, tracker: Option<&Tracker>, screen: bool, fresh: bool) -> bool {
        if self.seen {
            return true;
        }
        if let Some(t) = tracker {
            self.spec = Some(t.provider().spec());
        }
        // Follow the tracker: a /clear or another conversation picked inside
        // the provider moves its writes, limits included, to a new transcript.
        let session_id = tracker.and_then(Tracker::known_id);
        let current = tracker.and_then(Tracker::transcript);
        if self.session_id.as_deref() != session_id
            || self.main.as_ref().map(|tail| &tail.path) != current.as_ref()
        {
            self.session_id = session_id.map(str::to_owned);
            self.main = current.map(|path| Tail::found(path, self.launched_wall));
            self.subagents.clear();
            self.scanned = None;
            self.pending = None;
        }
        if let (Some(tail), Some(spec)) = (&mut self.main, self.spec) {
            self.seen = std::fs::metadata(&tail.path)
                .is_ok_and(|metadata| tail.read(self.launched_wall, spec, &metadata));
            let dir = tail.path.with_extension("").join("subagents");
            self.seen |= self.subagents_limited(&dir, fresh);
            return self.seen;
        }
        self.screen_seen |= screen && self.launched.elapsed() >= UNCONFIRMED;
        self.screen_seen
    }

    fn quota_evidence(&self, mut screen: ScreenLimits) -> ScreenLimits {
        // The active choice menu can appear immediately on resume. The
        // compaction message by itself can be part of the startup redraw.
        screen.claude_compaction &= self.launched.elapsed() >= UNCONFIRMED;
        screen.notice = false;
        screen
    }

    pub(crate) fn check_quota(
        &mut self,
        screen: ScreenLimits,
        input: impl Fn() -> InputActivity,
        fetch: impl FnOnce() -> Option<usage::Cached>,
        now: impl FnOnce() -> u64,
    ) -> bool {
        let evidence = self.quota_evidence(screen);
        if self.pending.is_none() && !evidence.any() {
            return false;
        }
        let activity = input();
        if activity.waiting {
            self.pending = None;
            return false;
        }
        let transcript_version = || {
            let tail = self.main.as_ref()?;
            let metadata = std::fs::symlink_metadata(&tail.path).ok()?;
            metadata
                .is_file()
                .then(|| TranscriptVersion::from_metadata(&metadata))
        };
        let transcript = transcript_version();
        // The blocked provider may draw its dialog only once. Recheck quota
        // until it is corroborated, or input or transcript activity supersedes
        // that screen evidence.
        if self.pending.as_ref().is_some_and(|pending| {
            pending.last_input != activity.last_forwarded || pending.transcript != transcript
        }) {
            self.pending = None;
        }
        if evidence.any() {
            self.pending
                .get_or_insert_with(|| PendingScreen {
                    evidence: ScreenLimits::default(),
                    last_input: activity.last_forwarded,
                    transcript,
                    retry: Instant::now(),
                })
                .evidence
                .merge(evidence);
        }
        let Some(pending) = &mut self.pending else {
            return false;
        };
        if Instant::now() < pending.retry {
            return false;
        }
        pending.retry = Instant::now() + Duration::from_secs(10);
        let cache = fetch();
        // Fetching quota can wait on the provider or the owner. The child can
        // write to its transcript and the user can queue input during that time.
        if input() != activity || transcript_version() != pending.transcript {
            self.pending = None;
            return false;
        }
        cache.is_some_and(|cache| corroborates(pending.evidence, &cache, now()))
    }
}

fn corroborates(screen: ScreenLimits, cache: &usage::Cached, at: u64) -> bool {
    matches!(cache.error.as_deref(), None | Some("provider-limit"))
        && cache.fetched_at != 0
        && at.saturating_sub(cache.fetched_at) < usage::TTL
        && cache.usage.windows.iter().any(|window| {
            window.used_percent >= 100.0
                && window.resets_at.is_none_or(|reset| reset > at)
                && (screen.claude_menu
                    || (screen.claude_compaction
                        && matches!(window.name.as_str(), "seven_day" | "weekly")))
        })
}

impl Limits {
    /// Whether a subagent transcript under `dir` (`<id>/subagents/`, up to
    /// two levels deep) written since launch gained a limit entry. Looked at
    /// once a second, or now when `fresh`.
    fn subagents_limited(&mut self, dir: &std::path::Path, fresh: bool) -> bool {
        if !fresh && self.scanned.is_some_and(|at| at.elapsed() < SCAN) {
            return false;
        }
        self.scanned = Some(Instant::now());
        let since = self.launched_wall;
        let Some(spec) = self.spec else {
            return false;
        };
        let mut files = Vec::new();
        subagent_files(dir, 0, since, &mut files);
        let mut found = false;
        for (path, metadata) in files {
            let tail = self
                .subagents
                .entry(path.clone())
                .or_insert_with(|| Tail::found(path, since));
            found |= tail.read(since, spec, &metadata);
        }
        found
    }
}

/// How often subagent transcripts are looked for.
const SCAN: Duration = Duration::from_secs(1);

/// `.jsonl` files under `dir`, at most two levels down, modified since `since`:
/// `subagents/agent-*.jsonl` and workflow agents'
/// `subagents/workflows/wf_*/agent-*.jsonl`.
fn subagent_files(
    dir: &std::path::Path,
    depth: usize,
    since: SystemTime,
    out: &mut Vec<(PathBuf, Metadata)>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if kind.is_dir() && depth < 2 {
            subagent_files(&path, depth + 1, since, out);
        } else if kind.is_file()
            && path.extension().is_some_and(|e| e == "jsonl")
            && let Ok(metadata) = entry.metadata()
            && metadata.modified().is_ok_and(|at| at >= since)
        {
            out.push((path, metadata));
        }
    }
}

impl Tail {
    fn at(path: PathBuf, offset: u64, skipping: bool) -> Self {
        let anchor = File::open(&path)
            .ok()
            .and_then(|mut file| anchor(&mut file, offset))
            .unwrap_or_default();
        Self {
            path,
            read_stamp: None,
            offset,
            anchor,
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
        // Only a tail can start inside a line; the end of a file never does.
        Self::at(path, offset, written && offset > 0)
    }

    /// Read what was appended since the last read; whether it holds a limit
    /// entry stamped at or after `since`.
    fn read(
        &mut self,
        since: SystemTime,
        spec: &dyn crate::provider::Spec,
        metadata: &Metadata,
    ) -> bool {
        if self.read_stamp.as_ref() == Some(&TranscriptVersion::from_metadata(metadata)) {
            return false;
        }
        self.read_stamp = None;
        let Ok(mut file) = File::open(&self.path) else {
            return false;
        };
        let Ok(metadata) = file.metadata() else {
            return false;
        };
        let len = metadata.len();
        if len < self.offset
            || anchor(&mut file, self.offset).is_none_or(|bytes| bytes != self.anchor)
        {
            // Truncated or rewritten: read it again from its start (entries
            // from before launch are told apart by their timestamps).
            self.offset = 0;
            self.anchor.clear();
            self.partial.clear();
            self.skipping = false;
        }
        if len == self.offset {
            self.read_stamp = Some(TranscriptVersion::from_metadata(&metadata));
            return false;
        }
        if file.seek(SeekFrom::Start(self.offset)).is_err() {
            return false;
        }
        let mut chunk = Vec::new();
        let Ok(n) = (&mut file).take(STEP).read_to_end(&mut chunk) else {
            return false;
        };
        self.offset += n as u64;
        if self.offset == len {
            self.read_stamp = Some(TranscriptVersion::from_metadata(&metadata));
        }
        self.anchor
            .extend_from_slice(&chunk[chunk.len().saturating_sub(ANCHOR)..]);
        let excess = self.anchor.len().saturating_sub(ANCHOR);
        self.anchor.drain(..excess);
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
                    .is_ok_and(|entry| spec.limit_entry(&entry, since));
            }
            self.partial.clear();
            self.skipping = false;
        }
        found
    }
}

/// A Claude transcript entry recording a usage-limit error at or after `since`.
pub fn limit_entry(entry: &Value, since: SystemTime) -> bool {
    crate::provider::Provider::Claude
        .spec()
        .limit_entry(entry, since)
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
    fn old_conversation_tails_are_discarded_while_the_new_file_is_pending() {
        let (home, old, mut tracker) = setup("");
        let mut limits = Limits::new(Some(&tracker));
        limits.launched_wall -= Duration::from_secs(1);
        let subagents = old.with_extension("").join("subagents");
        std::fs::create_dir_all(&subagents).unwrap();
        let subagent = subagents.join("agent-a1.jsonl");
        std::fs::write(&subagent, entry(SystemTime::now(), "server_error") + "\n").unwrap();
        assert!(!limits.check(Some(&tracker), false, true));
        assert!(limits.subagents.contains_key(&subagent));

        let pid = std::process::id();
        let other = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
        let registry = home.path().join("sessions").join(format!("{pid}.json"));
        crate::store::json_write(&registry, &serde_json::json!({"pid":pid,"sessionId":other}))
            .unwrap();
        tracker.observe(pid).unwrap();
        assert!(tracker.transcript().is_none());
        assert!(!limits.check(Some(&tracker), false, false));
        assert!(limits.main.is_none());
        assert!(limits.subagents.is_empty());
        assert!(limits.scanned.is_none());

        let new = old.with_file_name(format!("{other}.jsonl"));
        std::fs::write(&new, "{\"type\":\"user\"}\n").unwrap();
        for path in [old, subagent] {
            writeln!(
                File::options().append(true).open(path).unwrap(),
                "{}",
                entry(SystemTime::now(), "rate_limit"),
            )
            .unwrap();
        }
        tracker.observe(pid).unwrap();
        assert!(tracker.transcript().is_none());
        assert!(!limits.check(Some(&tracker), false, true));

        // A live screen diagnostic can refresh the pending registry transcript
        // immediately, without consulting the provider's descriptors.
        let mut current = File::options().append(true).open(new).unwrap();
        writeln!(current, "{}", entry(SystemTime::now(), "rate_limit")).unwrap();
        tracker.observe_now(pid).unwrap();
        assert!(limits.check(Some(&tracker), true, false));
    }

    #[test]
    fn the_first_line_appended_to_a_late_found_unchanged_transcript_counts() {
        // Resumed with --continue: the tracker learns the transcript only
        // after launch, and nothing was written to it since.
        let (_home, path, tracker) = setup("{\"type\":\"user\"}\n");
        std::thread::sleep(Duration::from_millis(20));
        let mut limits = Limits::new(None);
        std::thread::sleep(Duration::from_millis(5));
        assert!(!limits.check(Some(&tracker), false, false));
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all((entry(SystemTime::now(), "rate_limit") + "\n").as_bytes())
            .unwrap();
        assert!(limits.check(Some(&tracker), false, false));
    }

    #[test]
    fn a_transcript_rewritten_to_the_same_length_is_read_again() {
        let first = entry(hours_ago(4), "rate_limit") + "\n";
        let (_home, path, tracker) = setup(&first);
        let mut limits = Limits::new(Some(&tracker));
        assert!(!limits.check(Some(&tracker), false, false));
        // The rewrite keeps the old length and mtime.
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(Duration::from_millis(5));
        let live = entry(SystemTime::now(), "rate_limit") + "\n";
        assert_eq!(live.len(), first.len());
        let mut file = File::create(&path).unwrap();
        file.write_all(live.as_bytes()).unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
        assert!(limits.check(Some(&tracker), false, false));
    }

    #[test]
    fn a_limit_only_a_subagent_recorded_counts() {
        let (home, _path, tracker) = setup("");
        let mut limits = Limits::new(Some(&tracker));
        // A subagent's old limit and its live non-limit error don't count.
        let dir = home
            .path()
            .join("projects")
            .join("-synthetic")
            .join(ID)
            .join("subagents");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!limits.check(Some(&tracker), false, true));
        std::thread::sleep(Duration::from_millis(5));
        std::fs::write(
            dir.join("agent-a1.jsonl"),
            entry(hours_ago(4), "rate_limit")
                + "\n"
                + &entry(SystemTime::now(), "server_error")
                + "\n",
        )
        .unwrap();
        assert!(!limits.check(Some(&tracker), false, true));
        // A subagent of this launch hits the limit.
        std::fs::write(
            dir.join("agent-b2.jsonl"),
            entry(SystemTime::now(), "rate_limit") + "\n",
        )
        .unwrap();
        assert!(limits.check(Some(&tracker), false, true));
    }

    #[test]
    fn a_limit_a_workflow_agent_recorded_counts() {
        let (home, _path, tracker) = setup("");
        let mut limits = Limits::new(Some(&tracker));
        let workflow = home
            .path()
            .join("projects")
            .join("-synthetic")
            .join(ID)
            .join("subagents")
            .join("workflows")
            .join("wf_synthetic");
        // Deeper than any agent Claude writes: not looked at.
        let deeper = workflow.join("nested");
        std::fs::create_dir_all(&deeper).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        std::fs::write(
            deeper.join("agent-d4.jsonl"),
            entry(SystemTime::now(), "rate_limit") + "\n",
        )
        .unwrap();
        assert!(!limits.check(Some(&tracker), false, true));
        // A workflow agent of this launch hits the limit.
        std::fs::write(
            workflow.join("agent-c3.jsonl"),
            entry(SystemTime::now(), "rate_limit") + "\n",
        )
        .unwrap();
        assert!(limits.check(Some(&tracker), false, true));
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

    #[test]
    fn only_a_live_structured_compaction_error_counts_as_a_limit() {
        let mut entry = serde_json::json!({
            "type": "assistant",
            "isApiErrorMessage": true,
            "error": "prompt_too_long",
            "timestamp": stamp(hours_ago(4)),
            "message": { "content": [{
                "type": "text",
                "text": "Prompt is too long · automatic compaction failed: You've hit your weekly limit · resets Nov 22, 9am"
            }] }
        });
        let since = SystemTime::now() - Duration::from_secs(1);
        assert!(!limit_entry(&entry, since));
        entry["timestamp"] = stamp(SystemTime::now()).into();
        assert!(limit_entry(&entry, since));
        entry["isApiErrorMessage"] = false.into();
        assert!(!limit_entry(&entry, since));
        entry["isApiErrorMessage"] = true.into();
        entry["message"]["content"][0]["text"] =
            "Prompt is too long · automatic compaction failed: request exceeded its context window"
                .into();
        assert!(!limit_entry(&entry, since));
    }

    #[test]
    fn claude_screen_fallback_needs_fresh_exhausted_quota_for_the_window() {
        let mut limits = Limits::new(None);
        let compaction = ScreenLimits {
            claude_compaction: true,
            ..Default::default()
        };
        let menu = ScreenLimits {
            claude_menu: true,
            ..Default::default()
        };
        assert!(!limits.quota_evidence(compaction).any(), "startup redraw");
        assert!(limits.quota_evidence(menu).any(), "active choice menu");
        limits.launched -= UNCONFIRMED;
        assert!(limits.quota_evidence(compaction).any());
        let now = 10_000;
        let mut cache = usage::Cached {
            fetched_at: now - 1,
            usage: usage::Usage {
                windows: vec![usage::Window {
                    name: "five_hour".into(),
                    used_percent: 100.0,
                    resets_at: Some(now + 10),
                    duration_minutes: Some(300),
                    codex_slot: None,
                }],
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(!corroborates(compaction, &cache, now));
        assert!(corroborates(menu, &cache, now));
        cache.usage.windows[0].name = "seven_day".into();
        assert!(corroborates(compaction, &cache, now));
        assert!(!corroborates(compaction, &cache, now + 10), "passed reset");
        cache.usage.windows[0].used_percent = 99.0;
        assert!(!corroborates(compaction, &cache, now), "headroom");
        cache.usage.windows[0].used_percent = 100.0;
        cache.error = Some("network".into());
        assert!(!corroborates(compaction, &cache, now), "fetch failed");
        cache.error = Some("provider-limit".into());
        assert!(
            corroborates(compaction, &cache, now),
            "another run marked the limit"
        );
        cache.fetched_at = now - usage::TTL;
        assert!(!corroborates(compaction, &cache, now), "stale quota");
    }

    #[test]
    fn a_once_drawn_menu_can_be_corroborated_later_until_input_supersedes_it() {
        let mut limits = Limits::new(None);
        let menu = ScreenLimits {
            claude_menu: true,
            ..Default::default()
        };
        let now = 10_000;
        assert!(!limits.check_quota(menu, InputActivity::default, || None, || now));
        limits.pending.as_mut().unwrap().retry = Instant::now();
        assert!(limits.check_quota(
            ScreenLimits::default(),
            InputActivity::default,
            || Some(exhausted_quota(now, now + 10)),
            || now,
        ));
        limits.pending.as_mut().unwrap().retry = Instant::now();
        assert!(!limits.check_quota(
            ScreenLimits::default(),
            || InputActivity {
                last_forwarded: Some(Instant::now()),
                ..Default::default()
            },
            || panic!("new input superseded the blocked menu"),
            || now,
        ));
    }

    fn exhausted_quota(fetched_at: u64, reset: u64) -> usage::Cached {
        usage::Cached {
            fetched_at,
            usage: usage::Usage {
                windows: vec![usage::Window {
                    name: "weekly".into(),
                    used_percent: 100.0,
                    resets_at: Some(reset),
                    duration_minutes: Some(10080),
                    codex_slot: None,
                }],
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn transcript_activity_during_the_quota_fetch_supersedes_the_menu() {
        let (_home, path, tracker) = setup("");
        let mut limits = Limits::new(Some(&tracker));
        let menu = ScreenLimits {
            claude_menu: true,
            ..Default::default()
        };
        assert!(!limits.check_quota(
            menu,
            InputActivity::default,
            || {
                let entry = "{\"type\":\"assistant\",\"message\":\"work continued\"}\n";
                std::fs::write(&path, entry).unwrap();
                Some(exhausted_quota(10_000, 20_000))
            },
            || 10_000,
        ));
        assert!(limits.pending.is_none());
    }

    #[test]
    fn input_queued_during_the_quota_fetch_supersedes_the_menu() {
        let mut limits = Limits::new(None);
        let menu = ScreenLimits {
            claude_menu: true,
            ..Default::default()
        };
        let waiting = std::cell::Cell::new(false);
        assert!(!limits.check_quota(
            menu,
            || InputActivity {
                waiting: waiting.get(),
                ..Default::default()
            },
            || {
                waiting.set(true);
                Some(exhausted_quota(10_000, 20_000))
            },
            || 10_000,
        ));
        assert!(limits.pending.is_none());
    }

    #[test]
    fn quota_freshness_and_window_reset_are_checked_after_fetch() {
        let menu = ScreenLimits {
            claude_menu: true,
            ..Default::default()
        };
        let started_at = 10_000;
        let decision_at = started_at + 1;
        for cache in [
            exhausted_quota(decision_at - usage::TTL, decision_at + 10),
            exhausted_quota(started_at, decision_at),
        ] {
            assert!(corroborates(menu, &cache, started_at));
            let clock = std::cell::Cell::new(started_at);
            assert!(!Limits::new(None).check_quota(
                menu,
                InputActivity::default,
                || {
                    clock.set(decision_at);
                    Some(cache)
                },
                || clock.get(),
            ));
        }
    }
}
