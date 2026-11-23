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
/// A Codex limit notice this soon after a launch or resume may be a redraw
/// of an old one; it counts only if the account's quota confirms it.
pub const CODEX_EARLY: Duration = Duration::from_secs(15);
/// How long a Codex quota notice waits for a fresh quota read that confirms
/// it. A notice is only ever a trigger: text alone never switches accounts.
pub const CODEX_CONFIRM: Duration = Duration::from_secs(300);
/// Seconds after the notice at which the confirming read is (re)tried, while
/// the earlier ones failed or showed room (a quota reading can lag the
/// limit). Bounded by `CODEX_CONFIRM`.
const CODEX_RETRY: [u64; 6] = [0, 10, 30, 60, 120, 240];
/// How long one confirming read may take before it counts as failed.
const CODEX_READ_WAIT: Duration = Duration::from_secs(20);
/// After a failed exit with a notice unconfirmed: how long to keep reading.
pub const CODEX_EXIT_VERIFY: Duration = Duration::from_secs(30);

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
    pending: Option<PendingScreen>,
    /// agy: whether an early notice is still unconfirmed.
    early_unconfirmed: bool,
    /// agy: when the first early notice was seen (monotonic, wall clock).
    early_notice: Option<(Instant, SystemTime)>,
    late_started: bool,
    /// A Codex quota read running in the background.
    reading: Option<Reading>,
    /// A Codex quota notice waiting for a fresh read to confirm it.
    evidence: Option<Evidence>,
    /// `CODEX_EXIT_VERIFY`, shorter in tests.
    exit_verify: Duration,
}

/// A Codex quota notice seen on screen, not yet confirmed by quota.
struct Evidence {
    /// When it was seen (wall clock, seconds): a confirming read must be
    /// fetched at or after this.
    since: u64,
    seen: Instant,
    /// Reads started for it so far.
    attempts: usize,
    /// Whether any read came back with a reading (room or not).
    read: bool,
    /// Whether the last read failed (no answer, or a failed fetch).
    failed: bool,
}

struct Reading {
    result: std::sync::mpsc::Receiver<Option<usage::Cached>>,
    started: Instant,
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
            pending: None,
            early_unconfirmed: false,
            early_notice: None,
            late_started: false,
            reading: None,
            evidence: None,
            exit_verify: CODEX_EXIT_VERIFY,
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
        if let Some(tail) = &mut self.main {
            self.seen = std::fs::metadata(&tail.path)
                .is_ok_and(|metadata| tail.read(self.launched_wall, &metadata));
            let dir = tail.path.with_extension("").join("subagents");
            self.seen |= self.subagents_limited(&dir, fresh);
            return self.seen;
        }
        self.screen_seen |= screen && self.launched.elapsed() >= UNCONFIRMED;
        self.screen_seen
    }

    /// Whether a Codex usage limit is live. A quota notice on screen
    /// (`notice`) is only a trigger: it counts once a fresh quota read taken
    /// after it shows the account exhausted. Prose that looks like a notice,
    /// a redraw of an old one, or a model-specific bucket therefore never
    /// switches a healthy account. The read runs in the background (`fresh`,
    /// never on the terminal's thread) and is retried at `CODEX_RETRY` for up
    /// to `CODEX_CONFIRM`, since a reading can lag the limit; the evidence is
    /// kept across checks until then. A login rotated since launch
    /// (`rotated`) counts at once: Codex may be refusing with its old token,
    /// and the supervisor's credential recovery reloads the same account.
    ///
    /// After a failed exit (`gone`) with a notice unconfirmed, reads continue
    /// for up to `CODEX_EXIT_VERIFY` (the provider is gone; nothing waits)
    /// while they show room (a lagging reading); a read that fails then
    /// counts. While running, when no read at all came back (a follower
    /// without its owner, the quota service down) by the time the evidence
    /// runs out, the notice counts too: an agent is never left on an account
    /// it can't check.
    pub fn check_codex(
        &mut self,
        notice: bool,
        gone: bool,
        rotated: impl FnOnce() -> bool,
        fresh: impl Fn() -> std::sync::mpsc::Receiver<Option<usage::Cached>>,
        now: impl Fn() -> u64,
    ) -> bool {
        if self.screen_seen {
            return true;
        }
        if notice && self.evidence.is_none() {
            if rotated() {
                self.screen_seen = true;
                return true;
            }
            self.evidence = Some(Evidence {
                since: now(),
                seen: Instant::now(),
                attempts: 0,
                read: false,
                failed: false,
            });
        }
        self.confirm_codex(&fresh, &now);
        if gone && self.evidence.is_some() {
            // Re-read every few seconds, not on the retry schedule: nothing
            // else waits now, and the session's fate depends on it.
            let end = Instant::now() + self.exit_verify;
            let mut last: Option<Instant> = None;
            while !self.screen_seen && self.evidence.is_some() && Instant::now() < end {
                // The account can't be checked now: recover rather than leave
                // the session on it.
                if self.evidence.as_ref().is_some_and(|e| e.failed) {
                    self.screen_seen = true;
                    break;
                }
                if self.reading.is_none()
                    && last.is_none_or(|at| at.elapsed() >= Duration::from_secs(5))
                {
                    self.start_read(&fresh);
                    last = Some(Instant::now());
                }
                std::thread::sleep(Duration::from_millis(50));
                self.confirm_codex(&fresh, &now);
            }
            if self.evidence.as_ref().is_some_and(|e| !e.read) {
                self.screen_seen = true;
            }
        }
        self.screen_seen
    }

    fn start_read(
        &mut self,
        fresh: &impl Fn() -> std::sync::mpsc::Receiver<Option<usage::Cached>>,
    ) {
        if let Some(evidence) = &mut self.evidence {
            evidence.attempts += 1;
        }
        self.reading = Some(Reading {
            result: fresh(),
            started: Instant::now(),
        });
    }

    /// Collects a finished confirming read, starts the next one when due,
    /// and retires evidence that ran out.
    fn confirm_codex(
        &mut self,
        fresh: &impl Fn() -> std::sync::mpsc::Receiver<Option<usage::Cached>>,
        now: &impl Fn() -> u64,
    ) {
        use std::sync::mpsc::TryRecvError;
        let Some(since) = self.evidence.as_ref().map(|e| e.since) else {
            return;
        };
        if let Some(reading) = &self.reading {
            let result = match reading.result.try_recv() {
                Ok(result) => Some(result),
                Err(TryRecvError::Disconnected) => Some(None),
                Err(TryRecvError::Empty) if reading.started.elapsed() >= CODEX_READ_WAIT => {
                    Some(None)
                }
                Err(TryRecvError::Empty) => None,
            };
            if let Some(result) = result {
                self.reading = None;
                // A failed fetch comes back as a reading with its error.
                let good = result.filter(|cache| {
                    matches!(cache.error.as_deref(), None | Some("provider-limit"))
                });
                if let Some(evidence) = &mut self.evidence {
                    evidence.read |= good.is_some();
                    evidence.failed = good.is_none();
                }
                if let Some(cache) = good
                    && cache.fetched_at >= since
                    && exhausted(&cache, now())
                {
                    self.screen_seen = true;
                    self.evidence = None;
                    return;
                }
            }
        }
        let Some(evidence) = &self.evidence else {
            return;
        };
        if evidence.seen.elapsed() >= CODEX_CONFIRM {
            if self.reading.is_none() {
                // Never checked at all: count it. Checked and not confirmed:
                // it was prose, a redraw or a model bucket.
                self.screen_seen = !evidence.read;
                self.evidence = None;
            }
            return;
        }
        let due = CODEX_RETRY
            .get(evidence.attempts)
            .is_some_and(|after| evidence.seen.elapsed() >= Duration::from_secs(*after));
        if self.reading.is_none() && due {
            self.start_read(fresh);
        }
    }

    /// Whether an agy quota notice is live. agy has no quota reading to
    /// confirm against, so: after `CODEX_EARLY` a notice counts at once. One
    /// before that (possibly a redraw on resume) counts when agy exits with
    /// a failure, or when the window ends without progress since the notice
    /// (`progressed(at, wall)`: the transcript grew, or the user typed, after
    /// it). So a stuck, unattended agy is switched, and a redraw followed by
    /// normal work isn't.
    pub fn check_agy(
        &mut self,
        notice: bool,
        gone: bool,
        progressed: impl FnOnce(Instant, SystemTime) -> bool,
    ) -> bool {
        if self.screen_seen {
            return true;
        }
        if gone && (notice || self.early_unconfirmed) {
            self.screen_seen = true;
            return true;
        }
        let early = self.launched.elapsed() < CODEX_EARLY;
        if notice {
            if !early {
                self.screen_seen = true;
            } else if self.early_notice.is_none() {
                self.early_notice = Some((Instant::now(), SystemTime::now()));
                self.early_unconfirmed = true;
            }
        }
        if !early
            && self.early_unconfirmed
            && !self.late_started
            && let Some((at, wall)) = self.early_notice
        {
            self.late_started = true;
            self.screen_seen |= !progressed(at, wall);
        }
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

/// A fresh reading with some window used up and not yet reset.
fn exhausted(cache: &usage::Cached, at: u64) -> bool {
    matches!(cache.error.as_deref(), None | Some("provider-limit"))
        && cache.fetched_at != 0
        && at.saturating_sub(cache.fetched_at) < usage::TTL
        && cache.usage.windows.iter().any(|window| {
            window.used_percent >= 100.0 && window.resets_at.is_none_or(|reset| reset > at)
        })
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
        let mut files = Vec::new();
        subagent_files(dir, 0, since, &mut files);
        let mut found = false;
        for (path, metadata) in files {
            let tail = self
                .subagents
                .entry(path.clone())
                .or_insert_with(|| Tail::found(path, since));
            found |= tail.read(since, &metadata);
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
    fn read(&mut self, since: SystemTime, metadata: &Metadata) -> bool {
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
    if entry.get("isApiErrorMessage").and_then(Value::as_bool) != Some(true) {
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
        && match entry.get("error").and_then(Value::as_str) {
            Some("rate_limit") => true,
            Some("prompt_too_long") if entry["type"] == "assistant" => entry
                .pointer("/message/content")
                .and_then(Value::as_array)
                .is_some_and(|blocks| {
                    blocks.iter().any(|block| {
                        block["type"] == "text"
                            && block["text"].as_str().is_some_and(|text| {
                                crate::terminal::claude_compaction_limit(
                                    &text.to_lowercase().replace(['’', '‘'], "'"),
                                )
                            })
                    })
                }),
            _ => false,
        }
}

#[cfg(test)]
mod agy_tests {
    use super::*;

    fn launched(ago: Duration) -> Limits {
        let mut limits = Limits::new(None);
        limits.launched = Instant::now() - ago;
        limits
    }

    #[test]
    fn an_early_agy_notice_counts_unless_agy_moves_on() {
        let early = Duration::from_secs(2);
        let unasked = |_: Instant, _: SystemTime| -> bool { panic!("asked") };
        // A stuck, unattended agy: the notice, then nothing until the window
        // ends. It counts then.
        let mut limits = launched(early);
        assert!(!limits.check_agy(true, false, unasked));
        assert!(!limits.check_agy(false, false, unasked));
        limits.launched = Instant::now() - CODEX_EARLY;
        assert!(limits.check_agy(false, false, |_, _| false));
        // A redraw on resume, then normal work: the transcript grew after
        // the notice. Ignored.
        let mut limits = launched(early);
        assert!(!limits.check_agy(true, false, unasked));
        limits.launched = Instant::now() - CODEX_EARLY;
        let mut asked = None;
        assert!(!limits.check_agy(false, false, |at, _| {
            asked = Some(at);
            true
        }));
        assert!(asked.is_some());
        // Asked once only; shown again later, it counts.
        assert!(!limits.check_agy(false, false, unasked));
        assert!(limits.check_agy(true, false, unasked));
        // A failed exit after an early notice counts.
        let mut limits = launched(early);
        assert!(!limits.check_agy(true, false, unasked));
        assert!(limits.check_agy(false, true, unasked));
        // No notice: nothing, even at a failed exit or after the window.
        assert!(!launched(early).check_agy(false, true, unasked));
        assert!(!launched(CODEX_EARLY).check_agy(false, false, unasked));
        // After the window: at once.
        assert!(launched(CODEX_EARLY).check_agy(true, false, unasked));
    }
}

#[cfg(test)]
mod codex_tests {
    use super::*;

    fn reading(used: f64, at: u64) -> usage::Cached {
        usage::Cached {
            usage: usage::Usage {
                windows: vec![usage::Window {
                    name: "primary".into(),
                    used_percent: used,
                    resets_at: Some(at + 3600),
                    duration_minutes: Some(300),
                    codex_slot: None,
                }],
                ..Default::default()
            },
            fetched_at: at,
            ..Default::default()
        }
    }
    fn ready(value: Option<usage::Cached>) -> std::sync::mpsc::Receiver<Option<usage::Cached>> {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(value).unwrap();
        rx
    }
    /// A read that never answers (its sender is kept alive).
    fn stuck() -> std::sync::mpsc::Receiver<Option<usage::Cached>> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::mem::forget(tx);
        rx
    }

    #[test]
    fn a_codex_notice_counts_only_when_a_fresh_read_confirms_it() {
        let at = crate::now();
        let never = || -> std::sync::mpsc::Receiver<Option<usage::Cached>> { panic!("read") };
        let no = || false;
        let reads = std::cell::RefCell::new(Vec::<Option<usage::Cached>>::new());
        let script = |values: Vec<Option<usage::Cached>>| {
            *reads.borrow_mut() = values;
        };
        let fresh = || {
            let next = reads.borrow_mut().remove(0);
            ready(next)
        };
        let fresh_limits = || {
            let mut limits = Limits::new(None);
            limits.exit_verify = Duration::from_millis(300);
            limits
        };
        // Back-date the evidence so the next retry is due now.
        let later = |limits: &mut Limits, after: u64| {
            limits.evidence.as_mut().unwrap().seen = Instant::now() - Duration::from_secs(after);
        };

        // A real limit: the read taken after the notice has it exhausted.
        let mut limits = fresh_limits();
        script(vec![Some(reading(100.0, at))]);
        assert!(!limits.check_codex(true, false, no, fresh, || at));
        assert!(limits.check_codex(false, false, no, never, || at));

        // The login rotated since launch: counts at once, no read.
        assert!(fresh_limits().check_codex(true, false, || true, never, || at));

        // Prose, a redraw or a model bucket: the reads show room, so it
        // never counts, also after the evidence ran out; a later notice
        // starts over.
        let mut limits = fresh_limits();
        script(vec![Some(reading(20.0, at)); 6]);
        assert!(!limits.check_codex(true, false, no, fresh, || at));
        for after in [10, 30, 60, 120, 240] {
            assert!(!limits.check_codex(false, false, no, fresh, || at));
            later(&mut limits, after);
        }
        assert!(!limits.check_codex(false, false, no, fresh, || at));
        later(&mut limits, CODEX_CONFIRM.as_secs());
        assert!(!limits.check_codex(false, false, no, fresh, || at));
        assert!(limits.evidence.is_none());
        assert!(!limits.check_codex(false, false, no, never, || at));

        // The evidence is kept: a failed read, then a lagging one showing
        // room, then exhaustion, with no second notice (round 4, 1).
        let mut limits = fresh_limits();
        script(vec![
            None,
            Some(reading(99.0, at)),
            Some(reading(100.0, at + 30)),
        ]);
        assert!(!limits.check_codex(true, false, no, fresh, || at));
        assert!(!limits.check_codex(false, false, no, fresh, || at));
        later(&mut limits, 10);
        assert!(!limits.check_codex(false, false, no, fresh, || at));
        later(&mut limits, 30);
        assert!(!limits.check_codex(false, false, no, fresh, || at));
        assert!(limits.check_codex(false, false, no, never, || at + 30));

        // A reading fetched before the notice never confirms it, however
        // exhausted (round 4, 2: quota may have come back since).
        let mut limits = fresh_limits();
        script(vec![Some(reading(100.0, at - 60))]);
        assert!(!limits.check_codex(true, false, no, fresh, || at));
        assert!(!limits.check_codex(false, false, no, fresh, || at));

        // No read ever comes back: the evidence counts when it runs out.
        let mut limits = fresh_limits();
        assert!(!limits.check_codex(true, false, no, stuck, || at));
        limits.reading.as_mut().unwrap().started = Instant::now() - CODEX_READ_WAIT;
        later(&mut limits, CODEX_CONFIRM.as_secs());
        assert!(limits.check_codex(false, false, no, stuck, || at));

        // A failed exit with the notice unconfirmed reads again: confirmed,
        // it counts; showing room, it doesn't; never answered, it counts.
        let mut limits = fresh_limits();
        script(vec![None, Some(reading(100.0, at))]);
        assert!(!limits.check_codex(true, false, no, fresh, || at));
        limits.exit_verify = Duration::from_secs(10);
        assert!(limits.check_codex(false, true, no, fresh, || at));
        let mut limits = fresh_limits();
        script(vec![Some(reading(20.0, at)), Some(reading(20.0, at))]);
        assert!(!limits.check_codex(true, false, no, fresh, || at));
        assert!(!limits.check_codex(false, true, no, fresh, || at));
        let mut limits = fresh_limits();
        assert!(!limits.check_codex(true, false, no, stuck, || at));
        assert!(limits.check_codex(false, true, no, stuck, || at));

        // No notice: nothing happens, even at a failed exit.
        assert!(!fresh_limits().check_codex(false, true, no, never, || at));
        assert!(!fresh_limits().check_codex(false, false, no, never, || at));
    }
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
