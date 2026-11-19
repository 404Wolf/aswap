//! Pass-through auto-mode argv and session identity tracking. Transcript contents are
//! never read: only filenames, metadata, /proc descriptors and PID registry JSON.
use crate::{Error, Result, Safe, provider::Provider, store};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
pub fn valid_id(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}
pub struct AutoArgs {
    pub initial: Vec<String>,
    pub sticky: Vec<String>,
    pub id: Option<String>,
    pub one_shot: bool,
    pub ambiguous: bool,
}
impl AutoArgs {
    pub fn parse(provider: Provider, args: &[String]) -> Result<Self> {
        Self::parse_with_help(provider, args, "")
    }
    pub fn parse_with_help(provider: Provider, args: &[String], help: &str) -> Result<Self> {
        let mut schema = BTreeMap::new();
        for line in help.lines().map(str::trim).filter(|s| s.starts_with('-')) {
            let declaration = line.split("  ").next().unwrap_or(line);
            let arity = if declaration.contains("...") {
                2
            } else if declaration.contains('<') || declaration.contains('[') {
                1
            } else {
                0
            };
            for flag in declaration
                .split_whitespace()
                .map(|s| s.trim_end_matches(','))
                .filter(|s| s.starts_with('-'))
            {
                schema.insert(flag, arity);
            }
        }
        let mut ambiguous = false;
        let mut i = 0;
        let mut sticky = Vec::new();
        let mut id = None;
        let mut selection = false;
        let mut fork = false;
        let mut one_shot = false;
        if provider == Provider::Codex
            && args
                .first()
                .is_some_and(|s| matches!(s.as_str(), "resume" | "fork" | "exec"))
        {
            selection = true;
            one_shot = args[0] == "exec";
            fork = args[0] == "fork";
            i = 1;
            if let Some(value) = args.get(i).filter(|s| !s.starts_with('-')) {
                if valid_id(value) {
                    id = Some(value.clone());
                }
                i += 1;
            }
        }
        while i < args.len() {
            let flag = &args[i];
            let (key, inline) = flag
                .split_once('=')
                .map_or((flag.as_str(), None), |(a, b)| (a, Some(b)));
            if key == "--" {
                one_shot |= i + 1 < args.len();
                break;
            }
            let takes_session =
                provider == Provider::Claude && matches!(key, "--resume" | "-r" | "--session-id");
            if takes_session {
                selection = true;
                let value = inline.or_else(|| {
                    args.get(i + 1)
                        .filter(|s| !s.starts_with('-'))
                        .map(String::as_str)
                });
                if let Some(value) = value {
                    if valid_id(value) {
                        id = Some(value.into());
                    }
                    if inline.is_none() {
                        i += 1;
                    }
                }
            } else if (provider == Provider::Claude
                && matches!(key, "--continue" | "-c" | "--fork-session"))
                || (provider == Provider::Codex && key == "--last")
            {
                selection = true;
                fork |= key == "--fork-session";
            } else if provider == Provider::Claude && matches!(key, "--print" | "-p") {
                one_shot = true;
            } else if flag.starts_with('-') {
                sticky.push(flag.clone());
                // Arity descriptions are not an allowlist: every option passes
                // through. Unknown options conservatively retain their values.
                let boolean = matches!(
                    key,
                    "--dangerously-skip-permissions"
                        | "--allow-dangerously-skip-permissions"
                        | "--verbose"
                        | "--no-chrome"
                        | "--chrome"
                        | "--disable-slash-commands"
                        | "--no-alt-screen"
                        | "--no-daemon"
                        | "--search"
                        | "--safe-mode"
                        | "--bare"
                        | "--worktree"
                        | "--all"
                        | "--full-auto"
                        | "--dangerously-bypass-approvals-and-sandbox"
                        | "--include-non-interactive"
                        | "--strict-config"
                        | "--json"
                        | "--help"
                        | "-h"
                        | "--version"
                        | "-V"
                );
                let single = matches!(
                    key,
                    "--model"
                        | "-m"
                        | "--permission-mode"
                        | "--effort"
                        | "--settings"
                        | "--setting-sources"
                        | "--system-prompt"
                        | "--system-prompt-file"
                        | "--append-system-prompt"
                        | "--append-system-prompt-file"
                        | "--agent"
                        | "--agents"
                        | "--fallback-model"
                        | "--input-format"
                        | "--output-format"
                        | "--max-budget-usd"
                        | "--max-turns"
                        | "--debug-file"
                        | "--json-schema"
                        | "--profile"
                        | "--sandbox"
                        | "-s"
                        | "--ask-for-approval"
                        | "-a"
                        | "--cd"
                        | "-C"
                        | "--config"
                        | "--remote"
                        | "--remote-auth-token-env"
                ) || (provider == Provider::Codex && matches!(key, "-c" | "-p"));
                let described = schema.get(key).copied();
                let variadic = matches!(
                    key,
                    "--add-dir"
                        | "--mcp-config"
                        | "--tools"
                        | "--allowedTools"
                        | "--allowed-tools"
                        | "--disallowedTools"
                        | "--disallowed-tools"
                        | "--plugin-dir"
                        | "--image"
                        | "-i"
                );
                if inline.is_none() && described != Some(0) && !boolean {
                    if described == Some(1) || single {
                        if let Some(value) = args.get(i + 1) {
                            sticky.push(value.clone());
                            i += 1;
                        }
                    } else {
                        if !variadic
                            && described.is_none()
                            && args.get(i + 1).is_some_and(|s| !s.starts_with('-'))
                        {
                            ambiguous = true;
                        }
                        while args.get(i + 1).is_some_and(|s| !s.starts_with('-')) {
                            i += 1;
                            sticky.push(args[i].clone());
                        }
                    }
                }
            } else {
                one_shot = true;
            }
            i += 1;
        }
        let mut initial = args.to_vec();
        if provider == Provider::Claude && !selection {
            let mut bytes = [0u8; 16];
            use ring::rand::SecureRandom;
            ring::rand::SystemRandom::new()
                .fill(&mut bytes)
                .safe("cannot allocate session identity")?;
            bytes[6] = (bytes[6] & 15) | 64;
            bytes[8] = (bytes[8] & 63) | 128;
            let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
            let uuid = format!(
                "{}-{}-{}-{}-{}",
                &hex[..8],
                &hex[8..12],
                &hex[12..16],
                &hex[16..20],
                &hex[20..]
            );
            // Insert before '--' so a literal initial prompt remains untouched.
            let at = initial
                .iter()
                .position(|s| s == "--")
                .unwrap_or(initial.len());
            initial.splice(at..at, ["--session-id".into(), uuid.clone()]);
            id = Some(uuid);
        }
        if fork {
            // A fork creates a new conversation; its source UUID is not resumable identity.
            id = None;
        }
        Ok(Self {
            initial,
            sticky,
            id,
            one_shot,
            ambiguous,
        })
    }
    pub fn resume(
        &self,
        provider: Provider,
        id: &str,
        prompt: Option<&str>,
    ) -> Result<Vec<String>> {
        if self.ambiguous {
            return Err(Error(
                "provider option arity unavailable; refusing to replay ambiguous one-shot input",
            ));
        }
        if !valid_id(id) {
            return Err(Error(
                "exact session identity unavailable; refusing an automatic restart",
            ));
        }
        let mut args = match provider {
            Provider::Claude => vec!["--resume".into(), id.into()],
            Provider::Codex => vec!["resume".into(), id.into()],
        };
        args.extend(self.sticky.clone());
        if let Some(prompt) = prompt {
            args.extend(["--".into(), prompt.into()]);
        }
        Ok(args)
    }
}
const TRANSCRIPT_CHECK: Duration = Duration::from_secs(5);

#[cfg(test)]
thread_local! {
    static TRANSCRIPT_WALKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn transcript_depth(provider: Provider) -> usize {
    match provider {
        Provider::Claude => 1,
        Provider::Codex => 5,
    }
}

fn find_transcript(provider: Provider, root: &Path, id: &str) -> Result<Option<PathBuf>> {
    fn walk(
        provider: Provider,
        path: &Path,
        id: &str,
        depth: usize,
        found: &mut Option<PathBuf>,
    ) -> Result<()> {
        let entries = match fs::read_dir(path) {
            Err(err) if matches!(err.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
                return Ok(());
            }
            entries => entries.safe("cannot inspect session filenames")?,
        };
        for e in entries {
            let e = e.safe("cannot inspect session filename")?;
            let ty = e.file_type().safe("cannot inspect session metadata")?;
            if ty.is_dir() && depth < transcript_depth(provider) {
                walk(provider, &e.path(), id, depth + 1, found)?;
            } else if ty.is_file() && filename_id(provider, Path::new(&e.file_name())) == Some(id) {
                let path = e.path();
                // Match the previous lexicographic lookup if history has copies.
                if found.as_ref().is_none_or(|previous| path < *previous) {
                    *found = Some(path);
                }
            }
        }
        Ok(())
    }
    #[cfg(test)]
    TRANSCRIPT_WALKS.with(|walks| walks.set(walks.get() + 1));
    let mut found = None;
    walk(provider, root, id, 0, &mut found)?;
    Ok(found)
}
fn filename_id(provider: Provider, path: &Path) -> Option<&str> {
    let stem = path.file_stem()?.to_str()?;
    if path.extension()? != "jsonl" {
        return None;
    }
    let id = if provider == Provider::Codex {
        stem.get(stem.len().checked_sub(36)?..)?
    } else {
        stem
    };
    valid_id(id).then_some(id)
}

fn is_transcript_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file())
}

pub struct Tracker {
    provider: Provider,
    home: PathBuf,
    root: PathBuf,
    known: Option<String>,
    transcript: Option<PathBuf>,
    checked: Option<Instant>,
}
impl Tracker {
    pub fn new(provider: Provider, home: &Path, id: Option<String>) -> Result<Self> {
        let root = home.join(match provider {
            Provider::Claude => "projects",
            Provider::Codex => "sessions",
        });
        // A resume needs the pre-launch transcript as its baseline. If it
        // doesn't exist yet, give the running provider one immediate probe.
        let transcript = match id.as_deref() {
            Some(id) => find_transcript(provider, &root, id)?,
            None => None,
        };
        Ok(Self {
            provider,
            home: home.into(),
            root,
            known: id,
            checked: transcript.as_ref().map(|_| Instant::now()),
            transcript,
        })
    }
    pub fn observe(&mut self, pid: u32) -> Result<()> {
        // Codex's rollout writer may live in its app-server child. Inspect only
        // this process tree's open descriptors, never another terminal's files.
        fn process_files(pid: u32, depth: usize, out: &mut Vec<PathBuf>) {
            if depth > 5 {
                return;
            }
            if let Ok(dir) = fs::read_dir(format!("/proc/{pid}/fd")) {
                for e in dir.flatten() {
                    if let Ok(p) = fs::read_link(e.path()) {
                        out.push(p);
                    }
                }
            }
            if let Ok(children) = fs::read_to_string(format!("/proc/{pid}/task/{pid}/children")) {
                for child in children.split_whitespace().filter_map(|s| s.parse().ok()) {
                    process_files(child, depth + 1, out);
                }
            }
        }
        let mut descriptors = Vec::new();
        process_files(pid, 0, &mut descriptors);
        let owned: BTreeSet<_> = descriptors
            .iter()
            .filter(|p| p.starts_with(&self.root))
            .filter_map(|p| filename_id(self.provider, p))
            .collect();
        // Claude's PID registry identifies the main conversation even while
        // subagents hold other transcripts open or unrelated terminals write.
        let registered = if self.provider == Provider::Claude {
            let registry = self.home.join("sessions").join(format!("{pid}.json"));
            store::json_read::<Value>(&registry).ok().and_then(|v| {
                (v.get("pid").and_then(Value::as_u64) == Some(pid as u64))
                    .then(|| {
                        v.get("sessionId")
                            .and_then(Value::as_str)
                            .filter(|s| valid_id(s))
                            .map(str::to_owned)
                    })
                    .flatten()
            })
        } else {
            None
        };
        let known = if let Some(id) = registered.as_deref() {
            Some(id)
        } else if (self.provider == Provider::Claude && self.known.is_some()) || owned.is_empty() {
            // Shared history activity cannot contradict a known UUID. In
            // particular, an exited Codex has no descriptors left: another
            // terminal writing then must not strand its exact resume ID.
            // Claude's open files may belong to subagents; Codex can still
            // prove an actual session change with its owned rollout below.
            self.known.as_deref()
        } else if owned.len() == 1 {
            owned.first().copied()
        } else {
            None
        };
        if known != self.known.as_deref() {
            self.known = known.map(str::to_owned);
            self.transcript = None;
            self.checked = None;
        }
        let Some(id) = self.known.as_deref() else {
            return Ok(());
        };
        if self
            .checked
            .is_some_and(|at| at.elapsed() < TRANSCRIPT_CHECK)
        {
            return Ok(());
        }
        self.checked = Some(Instant::now());
        if self.transcript.as_deref().is_some_and(is_transcript_file) {
            return Ok(());
        }
        // An open descriptor can name the exact file without guessing how
        // Claude encodes project directories. Exclude deeper subagent paths.
        self.transcript = descriptors.into_iter().find(|path| {
            path.strip_prefix(&self.root).is_ok_and(|relative| {
                relative.components().count() <= transcript_depth(self.provider) + 1
                    && filename_id(self.provider, relative) == Some(id)
                    && is_transcript_file(path)
            })
        });
        if self.transcript.is_none() {
            self.transcript = find_transcript(self.provider, &self.root, id)?;
        }
        Ok(())
    }
    /// The cached path, if the conversation and its transcript are known.
    pub(crate) fn transcript(&self) -> Option<PathBuf> {
        self.transcript.clone()
    }
    /// Refresh the cache after provider exit, when no further `observe` will come.
    pub(crate) fn transcript_now(&mut self) {
        if let Some(id) = self.known.as_deref() {
            self.transcript = find_transcript(self.provider, &self.root, id)
                .ok()
                .flatten();
            self.checked = self.transcript.as_ref().map(|_| Instant::now());
        }
    }
    pub fn id(&self) -> Result<&str> {
        self.known.as_deref().ok_or(Error(
            "exact session identity unavailable; refusing an automatic restart",
        ))
    }
}

/// Help is never printed, and runs with a private config home and bounded output/time.
pub fn option_help(provider: Provider, view: &Path, args: &[String]) -> String {
    use std::{
        io::Read,
        os::unix::process::CommandExt,
        process::Stdio,
        time::{Duration, Instant},
    };
    let mut command = provider.command(view);
    if provider == Provider::Codex
        && args
            .first()
            .is_some_and(|s| matches!(s.as_str(), "resume" | "fork" | "exec"))
    {
        command.arg(&args[0]);
    }
    let Ok(mut child) = command
        .arg("--help")
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return String::new();
    };
    let output = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = output.take(262144).read_to_end(&mut bytes);
        bytes
    });
    let deadline = Instant::now() + Duration::from_secs(3);
    while child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    // SAFETY: the child owns this new process group; it has not been reaped unless exited.
    // kill only if still alive, to avoid reusing a reaped PID.
    if child.try_wait().ok().flatten().is_none() {
        unsafe {
            libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
        }
    }
    let _ = child.wait();
    if reader.is_finished() {
        String::from_utf8(reader.join().unwrap_or_default()).unwrap_or_default()
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tracker_tests {
    use super::*;
    const MAIN: &str = "11111111-2222-4333-8444-555555555555";
    const OTHER: &str = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";

    fn transcript(home: &Path, id: &str) -> PathBuf {
        let dir = home.join("projects").join("-synthetic");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{id}.jsonl"));
        fs::write(&path, "synthetic").unwrap();
        path
    }

    #[test]
    fn explicit_claude_identity_survives_other_terminal_and_subagent_writes() {
        let home = tempfile::tempdir().unwrap();
        let mut tracker = Tracker::new(Provider::Claude, home.path(), Some(MAIN.into())).unwrap();
        let other = transcript(home.path(), OTHER);
        tracker.observe(std::process::id()).unwrap();
        assert_eq!(tracker.id().unwrap(), MAIN);
        // A lone open transcript may belong to a tool/subagent, not the main session.
        let _held = fs::File::open(other).unwrap();
        tracker.observe(std::process::id()).unwrap();
        assert_eq!(tracker.id().unwrap(), MAIN);
    }

    #[test]
    fn claude_pid_registry_identifies_actual_session_change_despite_shared_writes() {
        let home = tempfile::tempdir().unwrap();
        let mut tracker = Tracker::new(Provider::Claude, home.path(), Some(MAIN.into())).unwrap();
        let main = transcript(home.path(), MAIN);
        let other = transcript(home.path(), OTHER);
        let _held = [
            fs::File::open(main).unwrap(),
            fs::File::open(&other).unwrap(),
        ];
        let pid = std::process::id();
        let registry = home.path().join("sessions").join(format!("{pid}.json"));
        store::json_write(&registry, &serde_json::json!({"pid":pid,"sessionId":OTHER})).unwrap();
        tracker.observe(pid).unwrap();
        assert_eq!(tracker.id().unwrap(), OTHER);
        assert_eq!(tracker.transcript(), Some(other));
    }

    #[test]
    fn steady_state_ticks_do_not_walk_and_a_removed_transcript_is_resolved_again() {
        let home = tempfile::tempdir().unwrap();
        let path = transcript(home.path(), MAIN);
        let walks = || TRANSCRIPT_WALKS.with(std::cell::Cell::get);
        let before = walks();
        let mut tracker = Tracker::new(Provider::Claude, home.path(), Some(MAIN.into())).unwrap();
        let pid = std::process::id();
        assert_eq!(tracker.transcript().as_ref(), Some(&path));
        assert_eq!(walks(), before + 1);

        let unrelated = transcript(home.path(), OTHER);
        for _ in 0..3 {
            fs::write(&unrelated, "unrelated changes").unwrap();
            tracker.observe(pid).unwrap();
            assert_eq!(tracker.transcript().as_ref(), Some(&path));
        }
        tracker.checked = Some(Instant::now() - TRANSCRIPT_CHECK);
        tracker.observe(pid).unwrap();
        assert_eq!(walks(), before + 1, "revalidation needs only a stat");

        let dir = home.path().join("projects/another-project");
        fs::create_dir(&dir).unwrap();
        let moved = dir.join(path.file_name().unwrap());
        fs::rename(&path, &moved).unwrap();
        tracker.checked = Some(Instant::now() - TRANSCRIPT_CHECK);
        tracker.observe(pid).unwrap();
        assert_eq!(tracker.transcript(), Some(moved));
        assert_eq!(walks(), before + 2);
    }

    #[test]
    fn a_missing_transcript_is_retried_on_interval_and_can_be_found_at_exit() {
        let home = tempfile::tempdir().unwrap();
        let mut tracker = Tracker::new(Provider::Claude, home.path(), Some(MAIN.into())).unwrap();
        let pid = std::process::id();
        tracker.observe(pid).unwrap();
        let walks = || TRANSCRIPT_WALKS.with(std::cell::Cell::get);
        let before = walks();
        let path = transcript(home.path(), MAIN);
        tracker.observe(pid).unwrap();
        assert!(tracker.transcript().is_none());
        assert_eq!(walks(), before);

        tracker.checked = Some(Instant::now() - TRANSCRIPT_CHECK);
        tracker.observe(pid).unwrap();
        assert_eq!(tracker.transcript().as_ref(), Some(&path));
        assert_eq!(walks(), before + 1);

        fs::remove_file(&path).unwrap();
        tracker.checked = Some(Instant::now() - TRANSCRIPT_CHECK);
        tracker.observe(pid).unwrap();
        assert!(tracker.transcript().is_none());
        transcript(home.path(), MAIN);
        tracker.transcript_now();
        assert_eq!(tracker.transcript(), Some(path));
    }

    #[test]
    fn fork_source_uuid_is_not_the_new_conversation_identity() {
        for (provider, args) in [
            (
                Provider::Claude,
                vec!["--resume".into(), MAIN.into(), "--fork-session".into()],
            ),
            (Provider::Codex, vec!["fork".into(), MAIN.into()]),
        ] {
            let parsed = AutoArgs::parse(provider, &args).unwrap();
            assert!(parsed.id.is_none());
            let home = tempfile::tempdir().unwrap();
            let tracker = Tracker::new(provider, home.path(), parsed.id).unwrap();
            assert!(tracker.id().is_err());
        }
    }

    fn rollout(home: &Path, id: &str) -> PathBuf {
        let dir = home.join("sessions");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("rollout-synthetic-{id}.jsonl"));
        fs::write(&path, "synthetic").unwrap();
        path
    }

    #[test]
    fn explicit_codex_identity_survives_other_sessions_writing_after_child_exit() {
        let home = tempfile::tempdir().unwrap();
        let mut tracker = Tracker::new(Provider::Codex, home.path(), Some(MAIN.into())).unwrap();
        let other = rollout(home.path(), OTHER);
        // No rollout is held by this child (as after it exits).
        tracker.observe(std::process::id()).unwrap();
        assert_eq!(tracker.id().unwrap(), MAIN);
        fs::write(other, "synthetic changed by another session").unwrap();
        tracker.observe(std::process::id()).unwrap();
        assert_eq!(tracker.id().unwrap(), MAIN);
    }

    #[test]
    fn codex_owned_rollouts_still_prove_changes_and_refuse_ambiguity() {
        let home = tempfile::tempdir().unwrap();
        let mut tracker = Tracker::new(Provider::Codex, home.path(), Some(MAIN.into())).unwrap();
        let other = rollout(home.path(), OTHER);
        let held = fs::File::open(&other).unwrap();
        let before = TRANSCRIPT_WALKS.with(std::cell::Cell::get);
        tracker.observe(std::process::id()).unwrap();
        assert_eq!(tracker.id().unwrap(), OTHER);
        assert_eq!(tracker.transcript(), Some(other));
        assert_eq!(TRANSCRIPT_WALKS.with(std::cell::Cell::get), before);
        let main = rollout(home.path(), MAIN);
        let _main_held = fs::File::open(main).unwrap();
        tracker.observe(std::process::id()).unwrap();
        assert!(
            tracker.id().is_err(),
            "two owned identities remain ambiguous"
        );
        assert!(tracker.transcript().is_none());
        drop(held);
    }

    #[test]
    fn unowned_codex_history_does_not_establish_an_unknown_identity() {
        let home = tempfile::tempdir().unwrap();
        let mut tracker = Tracker::new(Provider::Codex, home.path(), None).unwrap();
        rollout(home.path(), OTHER);
        tracker.observe(std::process::id()).unwrap();
        assert!(tracker.id().is_err());
    }
}
