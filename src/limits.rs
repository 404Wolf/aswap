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
    /// When the last confirming read started.
    last_read: Option<Instant>,
    /// Codex: a quota notice was seen this run (re-checked at a failed exit).
    noticed: bool,
    /// Codex: a notice came with a login rotated since launch.
    rotated_notice: bool,
    /// Reads that answered so far.
    answers: u64,
}

/// A Codex quota notice seen on screen, not yet confirmed by quota.
struct Evidence {
    seen: Instant,
    /// Reads started for it so far.
    attempts: usize,
    /// How the last read ended: Some(true) answered (room), Some(false)
    /// failed or timed out, None none yet.
    last: Option<bool>,
}

/// A fresh quota observation: one the provider answered for a read started
/// after the notice (`usage::fresh_read`), never a cached reading. None: the
/// read couldn't fetch or failed.
pub type Fresh = Option<usage::Usage>;

struct Reading {
    result: std::sync::mpsc::Receiver<Fresh>,
    started: Instant,
}
/// How long one read may run before it counts as failed and is replaced (a
/// hung worker must never stop the retries).
const CODEX_READ_WAIT: Duration = Duration::from_secs(60);

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
            last_read: None,
            noticed: false,
            rotated_notice: false,
            answers: 0,
        }
    }

    /// Whether this provider has hit a usage limit: its transcript gained a
    /// limit entry since launch, or, while no transcript is known, the screen
    /// (`screen`: shown since the last check) showed one `UNCONFIRMED` or
    /// more after launch. `fresh` checks subagents even if they were just
    /// scanned, for when the provider is gone.
    /// Whether the conversation's transcript is known (its limits are read
    /// there, not from the screen).
    pub fn transcript_known(&self) -> bool {
        self.main.is_some()
    }
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
    /// (`notice`) is only a trigger. It counts once a FRESH quota read
    /// (`fresh`: started after the notice, answered by the provider; see
    /// [`Fresh`]) shows the account exhausted: a window used up and not yet
    /// reset, or `ordinaryUsageAllowed` false. Prose that looks like a
    /// notice, a redraw or a model bucket never switches a healthy account.
    ///
    /// While the session runs, a notice NEVER counts without such a read.
    /// Reads run in the background, never on the terminal's thread, one at
    /// a time and each kept until it answers (a slow one is not abandoned
    /// for a second that would contend with it). They are retried at
    /// `CODEX_RETRY`, then every `CODEX_RETRY`'s last interval for as long as
    /// none answers (the session is still working, so nothing is stuck). The
    /// trigger is kept until an answer resolves it: confirmed, or showing
    /// room once `CODEX_CONFIRM` has passed.
    ///
    /// A login rotated since launch (`rotated`) never counts on its own; it
    /// lets the supervisor reload the same account (`rotated_notice`), never
    /// switch to another.
    ///
    /// After a failed exit (`gone`) following any notice this run, the
    /// account is re-read every 5 s for the whole `CODEX_EXIT_VERIFY`
    /// (stopped early by `cancelled`, a signal to the supervisor, which then
    /// counts nothing). Confirmed: it counts. Only if none of those reads
    /// ever answered does the notice count, after the whole window, so an
    /// exited session isn't ended on an account that can't be checked.
    #[allow(clippy::too_many_arguments)]
    pub fn check_codex(
        &mut self,
        notice: bool,
        gone: bool,
        rotated: impl FnOnce() -> bool,
        fresh: impl Fn() -> std::sync::mpsc::Receiver<Fresh>,
        cancelled: impl Fn() -> bool,
        now: impl Fn() -> u64,
    ) -> bool {
        if self.screen_seen {
            return true;
        }
        if notice {
            self.noticed = true;
            if self.evidence.is_none() {
                self.rotated_notice |= rotated();
                self.evidence = Some(Evidence {
                    seen: Instant::now(),
                    attempts: 0,
                    last: None,
                });
            }
        }
        let answers = self.answers;
        self.confirm_codex(&fresh, &now);
        if gone && self.noticed && !self.screen_seen {
            // Re-check after the exit, whatever the earlier reads said.
            self.evidence.get_or_insert(Evidence {
                seen: Instant::now(),
                attempts: 0,
                last: None,
            });
            let end = Instant::now() + self.exit_verify;
            let mut last: Option<Instant> = None;
            while !self.screen_seen && Instant::now() < end {
                if cancelled() {
                    return false;
                }
                if self.reading.is_none()
                    && last.is_none_or(|at| at.elapsed() >= Duration::from_secs(5))
                {
                    self.start_read(&fresh);
                    last = Some(Instant::now());
                }
                std::thread::sleep(Duration::from_millis(50));
                self.collect_codex(&now);
            }
            // Any answer since this check began (one taken at its start too)
            // means the account could be checked.
            if !self.screen_seen && self.answers == answers {
                self.screen_seen = true;
            }
        }
        self.screen_seen
    }

    /// A notice whose login rotated since launch: the supervisor may reload
    /// the same account with its new login (never switch to another).
    pub fn rotated_notice(&self) -> bool {
        self.rotated_notice && !self.screen_seen
    }

    fn start_read(&mut self, fresh: &impl Fn() -> std::sync::mpsc::Receiver<Fresh>) {
        if let Some(evidence) = &mut self.evidence {
            evidence.attempts += 1;
        }
        self.last_read = Some(Instant::now());
        self.reading = Some(Reading {
            result: fresh(),
            started: Instant::now(),
        });
    }

    /// Takes a finished read's answer, if any: Some(true) when it answered,
    /// Some(false) when it couldn't, None while it runs. Confirms on
    /// exhaustion.
    fn collect_codex(&mut self, now: &impl Fn() -> u64) -> Option<bool> {
        use std::sync::mpsc::TryRecvError;
        let reading = self.reading.as_ref()?;
        let result = match reading.result.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Disconnected) => None,
            // Hung: abandon it (its thread ends on its own) so retries go on.
            Err(TryRecvError::Empty) if reading.started.elapsed() >= CODEX_READ_WAIT => None,
            Err(TryRecvError::Empty) => return None,
        };
        self.reading = None;
        let Some(usage) = result else {
            if let Some(evidence) = &mut self.evidence {
                evidence.last = Some(false);
            }
            return Some(false);
        };
        self.answers += 1;
        if let Some(evidence) = &mut self.evidence {
            evidence.last = Some(true);
        }
        if exhausted_now(&usage, now()) {
            self.screen_seen = true;
            self.evidence = None;
        }
        Some(true)
    }

    /// Collects a finished confirming read, starts the next one when due,
    /// and retires a trigger an answer resolved.
    fn confirm_codex(
        &mut self,
        fresh: &impl Fn() -> std::sync::mpsc::Receiver<Fresh>,
        now: &impl Fn() -> u64,
    ) {
        if self.evidence.is_none() {
            return;
        }
        self.collect_codex(now);
        let Some(evidence) = &self.evidence else {
            return;
        };
        if self.reading.is_some() {
            return;
        }
        if evidence.last == Some(true) && evidence.seen.elapsed() >= CODEX_CONFIRM {
            // The latest read answered with room after the window: prose, a
            // redraw or a model bucket. A latest read that failed keeps the
            // trigger (a lagging limit may still show).
            self.evidence = None;
            return;
        }
        let last = *CODEX_RETRY.last().expect("retry schedule");
        let due = match CODEX_RETRY.get(evidence.attempts) {
            Some(after) => evidence.seen.elapsed() >= Duration::from_secs(*after),
            None => self
                .last_read
                .is_none_or(|at| at.elapsed() >= Duration::from_secs(last)),
        };
        if due {
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
/// A fresh observation showing the account out of usage now: a window used
/// up and not yet reset, or Codex's backend denying ordinary usage.
fn exhausted_now(usage: &usage::Usage, at: u64) -> bool {
    usage.ordinary_usage == usage::OrdinaryUsage::Denied
        || usage.windows.iter().any(|window| {
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

    fn obs(used: f64, at: u64) -> usage::Usage {
        usage::Usage {
            windows: vec![usage::Window {
                name: "primary".into(),
                used_percent: used,
                resets_at: Some(at + 3600),
                duration_minutes: Some(300),
                codex_slot: None,
            }],
            ..Default::default()
        }
    }
    fn ready(value: Fresh) -> std::sync::mpsc::Receiver<Fresh> {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(value).unwrap();
        rx
    }
    /// A read that never answers (its sender is kept alive).
    fn stuck() -> std::sync::mpsc::Receiver<Fresh> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::mem::forget(tx);
        rx
    }

    #[test]
    fn a_codex_notice_counts_only_when_a_fresh_read_confirms_it() {
        let at = crate::now();
        let never = || -> std::sync::mpsc::Receiver<Fresh> { panic!("read") };
        let no = || false;
        let reads = std::cell::RefCell::new(Vec::<Fresh>::new());
        let started = std::cell::Cell::new(0usize);
        let script = |values: Vec<Fresh>| {
            *reads.borrow_mut() = values;
            started.set(0);
        };
        let fresh = || {
            started.set(started.get() + 1);
            ready(reads.borrow_mut().remove(0))
        };
        let fresh_limits = || {
            let mut limits = Limits::new(None);
            limits.exit_verify = Duration::from_millis(400);
            limits
        };
        let later = |limits: &mut Limits, after: u64| {
            limits.evidence.as_mut().unwrap().seen = Instant::now() - Duration::from_secs(after);
            limits.last_read = Some(Instant::now() - Duration::from_secs(after));
        };

        // A real limit: the fresh read has the account exhausted.
        let mut limits = fresh_limits();
        script(vec![Some(obs(100.0, at))]);
        assert!(!limits.check_codex(true, false, no, fresh, no, || at));
        assert!(limits.check_codex(false, false, no, never, no, || at));
        assert_eq!(started.get(), 1);

        // Codex's backend denying ordinary usage is exhaustion too, however
        // the percentages read (review (c)).
        let mut limits = fresh_limits();
        let mut denied = obs(20.0, at);
        denied.ordinary_usage = usage::OrdinaryUsage::Denied;
        script(vec![Some(denied)]);
        assert!(!limits.check_codex(true, false, no, fresh, no, || at));
        assert!(limits.check_codex(false, false, no, never, no, || at));

        // A rotated login never counts by itself (review (a)): it only lets
        // the supervisor reload the same account.
        let mut limits = fresh_limits();
        script(vec![Some(obs(20.0, at))]);
        assert!(!limits.check_codex(true, false, || true, fresh, no, || at));
        assert!(limits.rotated_notice());
        assert!(!limits.check_codex(false, false, no, never, no, || at));

        // Prose, a redraw or a model bucket: fresh reads show room. Never
        // counts; retired once answered and past the window; no more reads.
        let mut limits = fresh_limits();
        script(vec![Some(obs(20.0, at)); 6]);
        assert!(!limits.check_codex(true, false, no, fresh, no, || at));
        for after in [10, 30, 60, 120, 240] {
            assert!(!limits.check_codex(false, false, no, fresh, no, || at));
            later(&mut limits, after);
        }
        assert!(!limits.check_codex(false, false, no, fresh, no, || at));
        later(&mut limits, CODEX_CONFIRM.as_secs());
        assert!(!limits.check_codex(false, false, no, fresh, no, || at));
        assert!(limits.evidence.is_none());
        assert!(!limits.check_codex(false, false, no, never, no, || at));

        // Kept across failures and lag, without a second notice.
        let mut limits = fresh_limits();
        script(vec![None, Some(obs(99.0, at)), Some(obs(100.0, at))]);
        assert!(!limits.check_codex(true, false, no, fresh, no, || at));
        assert!(!limits.check_codex(false, false, no, fresh, no, || at));
        later(&mut limits, 10);
        assert!(!limits.check_codex(false, false, no, fresh, no, || at));
        later(&mut limits, 30);
        assert!(!limits.check_codex(false, false, no, fresh, no, || at));
        assert!(limits.check_codex(false, false, no, never, no, || at));

        // While the session runs, failed or unanswered reads never make a
        // notice count, also long past the window; the trigger is kept and
        // retried (review (d) and the ruling).
        let mut limits = fresh_limits();
        script(vec![None; 10]);
        assert!(!limits.check_codex(true, false, no, fresh, no, || at));
        for after in [10, 30, 60, 120, 240, 600, 900] {
            later(&mut limits, after);
            assert!(!limits.check_codex(false, false, no, fresh, no, || at));
        }
        assert!(limits.evidence.is_some());
        let mut limits = fresh_limits();
        assert!(!limits.check_codex(true, false, no, stuck, no, || at));
        later(&mut limits, CODEX_CONFIRM.as_secs() * 2);
        assert!(!limits.check_codex(false, false, no, never, no, || at));
        assert!(
            limits.evidence.is_some() && limits.reading.is_some(),
            "one read, kept"
        );

        // A failed exit re-reads: confirmed counts, room doesn't.
        let mut limits = fresh_limits();
        script(vec![None, Some(obs(100.0, at))]);
        assert!(!limits.check_codex(true, false, no, fresh, no, || at));
        limits.exit_verify = Duration::from_secs(10);
        assert!(limits.check_codex(false, true, no, fresh, no, || at));
        let mut limits = fresh_limits();
        script(vec![Some(obs(20.0, at)); 4]);
        assert!(!limits.check_codex(true, false, no, fresh, no, || at));
        assert!(!limits.check_codex(false, true, no, fresh, no, || at));
        // Retired earlier (room), then exhausted at the failed exit: counts.
        let mut limits = fresh_limits();
        script(vec![Some(obs(20.0, at)), Some(obs(100.0, at))]);
        assert!(!limits.check_codex(true, false, no, fresh, no, || at));
        later(&mut limits, CODEX_CONFIRM.as_secs());
        assert!(!limits.check_codex(false, false, no, never, no, || at));
        assert!(limits.evidence.is_none());
        assert!(limits.check_codex(false, true, no, fresh, no, || at));

        // Never answered (failing or stuck): counts at exit, but only after
        // the whole bounded re-read ran, never at once (the ruling).
        for failing in [true, false] {
            let mut limits = fresh_limits();
            limits.exit_verify = Duration::from_millis(600);
            let begun = Instant::now();
            let counted = if failing {
                script(vec![None; 4]);
                assert!(!limits.check_codex(true, false, no, fresh, no, || at));
                limits.check_codex(false, true, no, fresh, no, || at)
            } else {
                assert!(!limits.check_codex(true, false, no, stuck, no, || at));
                limits.check_codex(false, true, no, stuck, no, || at)
            };
            assert!(counted);
            assert!(
                begun.elapsed() >= Duration::from_millis(600),
                "failing {failing}"
            );
        }
        // A lagging 99% answer, then failures, then the window ends: the
        // trigger is kept (the latest read failed), and a later exhausted
        // answer still confirms it (review 7).
        let mut limits = fresh_limits();
        script(vec![
            Some(obs(99.0, at)),
            None,
            None,
            None,
            None,
            None,
            Some(obs(100.0, at)),
        ]);
        assert!(!limits.check_codex(true, false, no, fresh, no, || at));
        for after in [10, 30, 60, 120, 240, 600] {
            later(&mut limits, after);
            assert!(!limits.check_codex(false, false, no, fresh, no, || at));
        }
        assert!(limits.evidence.is_some(), "kept after a failed latest read");
        later(&mut limits, 900);
        assert!(limits.check_codex(false, false, no, never, no, || at));

        // A hung read is replaced after CODEX_READ_WAIT, so retries go on
        // (review 8).
        let mut limits = fresh_limits();
        assert!(!limits.check_codex(true, false, no, stuck, no, || at));
        limits.reading.as_mut().unwrap().started = Instant::now() - CODEX_READ_WAIT;
        later(&mut limits, 10);
        script(vec![Some(obs(100.0, at))]);
        assert!(!limits.check_codex(false, false, no, fresh, no, || at));
        assert_eq!(started.get(), 1, "a replacement read started");
        assert!(limits.check_codex(false, false, no, never, no, || at));

        // A room answer taken as the exit check begins counts as answered:
        // no fallback (review: the entry call's answer).
        let mut limits = fresh_limits();
        limits.reading = Some(Reading {
            result: ready(Some(obs(20.0, at))),
            started: Instant::now(),
        });
        limits.noticed = true;
        limits.evidence = Some(Evidence {
            seen: Instant::now(),
            attempts: 1,
            last: None,
        });
        script(vec![None; 4]);
        assert!(!limits.check_codex(false, true, no, fresh, no, || at));

        // A signal during that re-read stops it, counting nothing.
        let mut limits = fresh_limits();
        limits.exit_verify = Duration::from_secs(30);
        assert!(!limits.check_codex(true, false, no, stuck, no, || at));
        let begun = Instant::now();
        assert!(!limits.check_codex(false, true, no, stuck, || true, || at));
        assert!(begun.elapsed() < Duration::from_secs(1));

        // No notice: nothing happens, even at a failed exit.
        assert!(!fresh_limits().check_codex(false, true, no, never, no, || at));
        assert!(!fresh_limits().check_codex(false, false, no, never, no, || at));
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
