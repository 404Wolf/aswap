//! Pass-through auto-mode argv and session identity tracking. Transcript contents are
//! never read: only filenames, metadata, /proc descriptors and PID registry JSON.
use crate::{Error, Result, Safe, provider::Provider, shared, store};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{ErrorKind, Read},
    os::unix::fs::{FileExt, MetadataExt},
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
            // agy parses Go flags: `-name` is `--name`.
            let long;
            let key = if provider == Provider::Agy
                && key.len() > 2
                && key.starts_with('-')
                && !key.starts_with("--")
            {
                long = format!("-{key}");
                long.as_str()
            } else {
                key
            };
            if key == "--" {
                one_shot |= i + 1 < args.len();
                break;
            }
            let takes_session = (provider == Provider::Claude
                && matches!(key, "--resume" | "-r" | "--session-id"))
                || (provider == Provider::Agy && key == "--conversation");
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
                || (provider == Provider::Agy && matches!(key, "--continue" | "-c"))
            {
                selection = true;
                fork |= key == "--fork-session";
            } else if matches!(provider, Provider::Claude | Provider::Agy)
                && matches!(key, "--print" | "-p" | "--prompt")
            {
                one_shot = true;
            } else if provider == Provider::Agy && matches!(key, "--prompt-interactive" | "-i") {
                // An interactive session started with a first prompt: the
                // prompt is said once, never replayed on a restart.
                if inline.is_none() {
                    i += 1;
                }
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
                ) || (provider == Provider::Agy
                    && matches!(
                        key,
                        "--new-project" | "--remote-agent" | "--remote-control" | "--sandbox"
                    ));
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
                ) || (provider == Provider::Codex && matches!(key, "-c" | "-p"))
                    || (provider == Provider::Agy
                        && matches!(
                            key,
                            "--mode" | "--project" | "--log-file" | "--print-timeout"
                        ));
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
            Provider::Agy => vec!["--conversation".into(), id.into()],
        };
        args.extend(self.sticky.clone());
        if let Some(prompt) = prompt {
            if provider == Provider::Agy {
                // agy (Go flags) takes an interactive first prompt as a flag.
                args.push(format!("--prompt-interactive={prompt}"));
            } else {
                args.extend(["--".into(), prompt.into()]);
            }
        }
        Ok(args)
    }
}
const TRANSCRIPT_CHECK: Duration = Duration::from_secs(5);
const TRANSCRIPT_RETRY_MAX: Duration = Duration::from_secs(60);
// Fall back to a full scan periodically even when the process tree and fd
// counts are unchanged: a process can replace an fd without changing its count.
const FD_CHECK: Duration = Duration::from_secs(20);

#[cfg(test)]
thread_local! {
    static TRANSCRIPT_WALKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static FD_SCANS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn transcript_depth(provider: Provider) -> usize {
    match provider {
        Provider::Claude => 1,
        Provider::Codex => 5,
        // brain/<id>/.system_generated/logs/transcript.jsonl
        Provider::Agy => 4,
    }
}

fn find_transcript(provider: Provider, root: &Path, id: &str) -> Result<Option<PathBuf>> {
    if provider == Provider::Agy {
        // `root` is the shared antigravity-cli directory; the path is known.
        let path = crate::agy::transcript(root.parent().unwrap_or(root), id);
        return Ok(is_transcript_file(&path).then_some(path));
    }
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
    if provider == Provider::Agy {
        // agy holds presence/<id>.lock open for its conversation.
        if path.parent()?.file_name()? != "presence" || path.extension()? != "lock" {
            return None;
        }
        let id = path.file_stem()?.to_str()?;
        return valid_id(id).then_some(id);
    }
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

fn writable_proc_fd(path: &Path) -> bool {
    // Linux gives /proc/<pid>/fd symlinks the access mode of the open fd:
    // owner-writable (0o200) for an fd open for writing.
    fs::symlink_metadata(path).is_ok_and(|m| m.is_symlink() && m.mode() & 0o200 != 0)
}

fn fd_directory_state(path: &Path) -> Option<(u64, u64)> {
    // Since Linux 6.2, procfs reports the open fd count as this directory's size.
    let metadata = fs::metadata(path).ok()?;
    (metadata.len() != 0).then(|| (metadata.ino(), metadata.len()))
}

struct ProcessSnapshot {
    fd_dir: PathBuf,
    fd_state: (u64, u64),
    children: fs::File,
    child_ids: String,
    buffer: Vec<u8>,
}

impl ProcessSnapshot {
    fn unchanged(&mut self) -> bool {
        fd_directory_state(&self.fd_dir) == Some(self.fd_state)
            && self
                .children
                .read_at(&mut self.buffer, 0)
                .is_ok_and(|n| &self.buffer[..n] == self.child_ids.as_bytes())
    }
}

// Codex's rollout writer may live in its app-server child. Observe only this
// process tree, without treating unrelated history activity as ownership.
fn process_files(
    pid: u32,
    depth: usize,
    root: &Path,
    out: &mut Vec<(PathBuf, PathBuf)>,
    snapshots: &mut Option<Vec<ProcessSnapshot>>,
) {
    if depth > 5 {
        return;
    }
    let fd_dir = PathBuf::from(format!("/proc/{pid}/fd"));
    let mut children = fs::File::open(format!("/proc/{pid}/task/{pid}/children")).ok();
    // Snapshot the count before walking the descriptors so an open during the
    // walk still triggers another scan when the count changes.
    let fd_state = snapshots.as_ref().and_then(|_| fd_directory_state(&fd_dir));
    if let Ok(dir) = fs::read_dir(&fd_dir) {
        for e in dir.flatten() {
            let fd = e.path();
            if let Ok(path) = fs::read_link(&fd)
                && path.starts_with(root)
            {
                out.push((fd, path));
            }
        }
    } else {
        *snapshots = None;
    }
    let mut child_ids = String::new();
    if let Some(file) = &mut children
        && file.read_to_string(&mut child_ids).is_err()
    {
        children = None;
    }
    for child in child_ids.split_whitespace().filter_map(|s| s.parse().ok()) {
        process_files(child, depth + 1, root, out, snapshots);
    }
    if let Some(tree) = snapshots {
        if let (Some(fd_state), Some(children)) = (fd_state, children) {
            let buffer = vec![0; child_ids.len() + 1];
            tree.push(ProcessSnapshot {
                fd_dir,
                fd_state,
                children,
                child_ids,
                buffer,
            });
        } else {
            // A vanished process or a kernel without fd counts cannot be
            // watched cheaply. Keep doing full scans until a snapshot works.
            *snapshots = None;
        }
    }
}

pub struct Tracker {
    provider: Provider,
    home: PathBuf,
    root: PathBuf,
    known: Option<String>,
    transcript: Option<PathBuf>,
    next_check: Option<Instant>,
    retry_delay: Duration,
    registry: Option<(u32, shared::Stamp, String)>,
    last_fd_scan: Option<(u32, Instant)>,
    rollout_writer: Option<(PathBuf, PathBuf)>,
    process_tree: Option<Vec<ProcessSnapshot>>,
}
impl Tracker {
    pub fn new(provider: Provider, home: &Path, id: Option<String>) -> Result<Self> {
        let root = home.join(match provider {
            Provider::Claude => "projects",
            Provider::Codex => "sessions",
            Provider::Agy => crate::agy::APP,
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
            next_check: transcript
                .as_ref()
                .map(|_| Instant::now() + TRANSCRIPT_CHECK),
            retry_delay: TRANSCRIPT_CHECK,
            transcript,
            registry: None,
            last_fd_scan: None,
            rollout_writer: None,
            process_tree: None,
        })
    }
    pub fn observe(&mut self, pid: u32) -> Result<()> {
        self.observe_with(pid, false)
    }
    pub(crate) fn observe_now(&mut self, pid: u32) -> Result<()> {
        self.observe_with(pid, true)
    }
    fn observe_with(&mut self, pid: u32, fresh: bool) -> Result<()> {
        let now = Instant::now();
        if let Some(path) = &self.transcript
            && self.next_check.is_some_and(|at| at <= now)
        {
            if is_transcript_file(path) {
                self.next_check = Some(now + TRANSCRIPT_CHECK);
            } else {
                self.transcript = None;
            }
        }
        if self.provider == Provider::Claude {
            self.refresh_registry(pid);
        }
        let registered = self.registry.as_ref().map(|(_, _, id)| id.as_str());
        let fd_due = self.provider != Provider::Claude
            && (fresh
                || self.last_fd_scan.is_none_or(|(previous, at)| {
                    previous != pid || now.duration_since(at) >= FD_CHECK
                })
                || self.rollout_writer.as_ref().is_none_or(|(fd, path)| {
                    fs::read_link(fd).ok().as_ref() != Some(path) || !writable_proc_fd(fd)
                })
                || self
                    .process_tree
                    .as_mut()
                    .is_none_or(|tree| !tree.iter_mut().all(ProcessSnapshot::unchanged)));
        let scan = registered.is_none() && (self.transcript.is_none() || fd_due);
        let mut descriptors = Vec::new();
        if scan {
            self.last_fd_scan = Some((pid, now));
            #[cfg(test)]
            FD_SCANS.with(|scans| scans.set(scans.get() + 1));
            self.process_tree = (self.provider == Provider::Codex).then(Vec::new);
            process_files(pid, 0, &self.root, &mut descriptors, &mut self.process_tree);
        }
        let owned: BTreeSet<_> = descriptors
            .iter()
            .filter_map(|(_, path)| filename_id(self.provider, path))
            .collect();
        if scan && self.provider == Provider::Codex {
            // A retained history reader can outlive a switch to another
            // conversation. Throttle only while an owned writer is present.
            self.rollout_writer = owned.first().filter(|_| owned.len() == 1).and_then(|id| {
                descriptors
                    .iter()
                    .find(|(fd, path)| {
                        filename_id(self.provider, path) == Some(*id) && writable_proc_fd(fd)
                    })
                    .cloned()
            });
        }
        // Claude's PID registry identifies the main conversation even while
        // subagents hold other transcripts open or unrelated terminals write.
        let known = if let Some(id) = registered {
            Some(id)
        } else if (self.provider == Provider::Claude && self.known.is_some()) || owned.is_empty() {
            // Keep transcript attribution through an empty scan. Claude's
            // open files can belong to subagents, so an already known Claude
            // ID also takes precedence here.
            self.known.as_deref()
        } else if owned.len() == 1 {
            owned.first().copied()
        } else {
            None
        };
        if known != self.known.as_deref() {
            self.known = known.map(str::to_owned);
            self.transcript = None;
            self.next_check = None;
            self.retry_delay = TRANSCRIPT_CHECK;
        }
        let Some(id) = self.known.as_deref() else {
            return Ok(());
        };
        if self.transcript.is_some() {
            return Ok(());
        }
        // An open descriptor can name the exact file without guessing how
        // Claude encodes project directories, even between history lookups.
        // Exclude deeper subagent paths.
        // agy's descriptor is a lock, not the transcript.
        let direct = descriptors.into_iter().map(|(_, path)| path).find(|path| {
            self.provider != Provider::Agy
                && path.strip_prefix(&self.root).is_ok_and(|relative| {
                    relative.components().count() <= transcript_depth(self.provider) + 1
                        && filename_id(self.provider, relative) == Some(id)
                        && is_transcript_file(path)
                })
        });
        if let Some(path) = direct {
            self.cache_transcript(path, now);
        } else if (registered.is_some() && fresh) || self.next_check.is_none_or(|at| at <= now) {
            self.walk_for_transcript(now)?;
        }
        Ok(())
    }
    fn refresh_registry(&mut self, pid: u32) {
        // One stat per tick detects /clear and atomic replacement. Read the
        // JSON again only when it changes or the last read did not prove an ID.
        let path = self.home.join("sessions").join(format!("{pid}.json"));
        let Some(stamp) = shared::stamp(&path) else {
            self.registry = None;
            return;
        };
        if self
            .registry
            .as_ref()
            .is_some_and(|(previous, old, _)| *previous == pid && *old == stamp)
        {
            return;
        }
        self.registry = (|| {
            let value: Value = store::json_read(&path).ok()?;
            let id = value.get("sessionId")?.as_str()?;
            (value.get("pid")?.as_u64()? == u64::from(pid) && valid_id(id))
                .then(|| (pid, stamp, id.to_owned()))
        })();
    }
    /// The cached path, if the conversation and its transcript are known.
    pub(crate) fn transcript(&self) -> Option<PathBuf> {
        self.transcript.clone()
    }
    /// Refresh the cache after provider exit, when no further `observe` will come.
    pub(crate) fn transcript_now(&mut self) {
        let now = Instant::now();
        if self.transcript.as_deref().is_some_and(is_transcript_file) {
            self.next_check = Some(now + TRANSCRIPT_CHECK);
        } else {
            let _ = self.walk_for_transcript(now);
        }
    }
    fn walk_for_transcript(&mut self, now: Instant) -> Result<()> {
        let Some(id) = self.known_id() else {
            return Ok(());
        };
        match find_transcript(self.provider, &self.root, id) {
            Ok(Some(path)) => {
                self.cache_transcript(path, now);
                Ok(())
            }
            result => {
                self.transcript = None;
                self.next_check = Some(now + self.retry_delay);
                self.retry_delay = (self.retry_delay * 2).min(TRANSCRIPT_RETRY_MAX);
                result.map(|_| ())
            }
        }
    }
    fn cache_transcript(&mut self, path: PathBuf, now: Instant) {
        self.transcript = Some(path);
        self.next_check = Some(now + TRANSCRIPT_CHECK);
        self.retry_delay = TRANSCRIPT_CHECK;
    }
    pub(crate) fn known_id(&self) -> Option<&str> {
        self.known.as_deref()
    }
    pub fn id(&self) -> Result<&str> {
        self.known_id().ok_or(Error(
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
        store::json_write(&registry, &serde_json::json!({"pid":pid,"sessionId":MAIN})).unwrap();
        let scans = || FD_SCANS.with(std::cell::Cell::get);
        let before = scans();
        for _ in 0..3 {
            tracker.observe(pid).unwrap();
            assert_eq!(tracker.id().unwrap(), MAIN);
        }
        assert_eq!(scans(), before, "a valid registry avoids descriptor scans");

        store::json_write(&registry, &serde_json::json!({"pid":pid,"sessionId":OTHER})).unwrap();
        tracker.observe(pid).unwrap();
        assert_eq!(tracker.id().unwrap(), OTHER);
        assert_eq!(tracker.transcript(), Some(other));
        assert_eq!(scans(), before, "a registry switch also avoids a scan");
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
        tracker.next_check = Some(Instant::now());
        tracker.observe(pid).unwrap();
        assert_eq!(walks(), before + 1, "revalidation needs only a stat");

        let dir = home.path().join("projects/another-project");
        fs::create_dir(&dir).unwrap();
        let moved = dir.join(path.file_name().unwrap());
        fs::rename(&path, &moved).unwrap();
        tracker.next_check = Some(Instant::now());
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

        tracker.next_check = Some(Instant::now());
        tracker.observe(pid).unwrap();
        assert_eq!(tracker.transcript().as_ref(), Some(&path));
        assert_eq!(walks(), before + 1);

        fs::remove_file(&path).unwrap();
        tracker.next_check = Some(Instant::now());
        tracker.observe(pid).unwrap();
        assert!(tracker.transcript().is_none());
        transcript(home.path(), MAIN);
        tracker.transcript_now();
        assert_eq!(tracker.transcript(), Some(path));
    }

    #[test]
    fn missing_transcript_walks_back_off_until_an_open_descriptor_resolves_it() {
        let home = tempfile::tempdir().unwrap();
        let mut tracker = Tracker::new(Provider::Claude, home.path(), Some(MAIN.into())).unwrap();
        let pid = std::process::id();
        let walks = || TRANSCRIPT_WALKS.with(std::cell::Cell::get);
        let before = walks();
        for (i, seconds) in [5, 10, 20, 40, 60, 60].into_iter().enumerate() {
            let start = Instant::now();
            tracker.next_check = Some(start);
            tracker.observe(pid).unwrap();
            let delay = Duration::from_secs(seconds);
            let deadline = tracker.next_check.unwrap();
            assert!(deadline >= start + delay && deadline <= Instant::now() + delay);
            tracker.observe(pid).unwrap();
            assert_eq!(walks(), before + i + 1);
        }
        let path = transcript(home.path(), MAIN);
        let _held = fs::File::open(&path).unwrap();
        tracker.observe(pid).unwrap();
        assert_eq!(tracker.transcript(), Some(path));
        assert_eq!(walks(), before + 6);
        assert_eq!(tracker.retry_delay, TRANSCRIPT_CHECK);
        tracker.transcript_now();
        assert_eq!(walks(), before + 6, "exit can reuse the existing path");
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

    fn rollout_writer(path: &Path) -> fs::File {
        fs::OpenOptions::new().append(true).open(path).unwrap()
    }

    struct RolloutProcess {
        child: std::process::Child,
        output: std::io::BufReader<std::process::ChildStdout>,
    }

    impl RolloutProcess {
        fn start(main: &Path, other: &Path) -> Self {
            use std::process::{Command, Stdio};
            let program = r#"import sys
with open(sys.argv[1], 'a') as first:
    second = None
    print('ready', flush=True)
    for command in sys.stdin:
        if command.strip() == 'open':
            second = open(sys.argv[2], 'a')
        elif command.strip() == 'close':
            first.close()
            if second is not None:
                second.close()
        print('ready', flush=True)
"#;
            let mut child = Command::new("python3")
                .arg("-c")
                .arg(program)
                .args([main, other])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            let output = std::io::BufReader::new(child.stdout.take().unwrap());
            let mut process = Self { child, output };
            process.ready();
            process
        }

        fn ready(&mut self) {
            use std::io::BufRead;
            let mut line = String::new();
            self.output.read_line(&mut line).unwrap();
            assert_eq!(line, "ready\n");
        }

        fn send(&mut self, command: &str) {
            use std::io::Write;
            writeln!(self.child.stdin.as_mut().unwrap(), "{command}").unwrap();
            self.ready();
        }
    }

    impl Drop for RolloutProcess {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
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
        let held = rollout_writer(&other);
        let before = TRANSCRIPT_WALKS.with(std::cell::Cell::get);
        tracker.observe(std::process::id()).unwrap();
        assert_eq!(tracker.id().unwrap(), OTHER);
        assert_eq!(tracker.transcript(), Some(other));
        assert_eq!(TRANSCRIPT_WALKS.with(std::cell::Cell::get), before);
        let main = rollout(home.path(), MAIN);
        let main_held = rollout_writer(&main);

        // Recovery rechecks even before the cadence expires.
        tracker.observe_now(std::process::id()).unwrap();
        assert!(
            tracker.id().is_err(),
            "two owned identities remain ambiguous"
        );
        assert!(tracker.transcript().is_none());
        drop(held);
        tracker.observe(std::process::id()).unwrap();
        assert_eq!(
            tracker.id().unwrap(),
            MAIN,
            "retry immediately when unresolved"
        );
        let _held = rollout_writer(&rollout(home.path(), OTHER));
        let pid = std::process::id();
        tracker.last_fd_scan = Some((pid, Instant::now() - FD_CHECK));
        tracker.observe(pid).unwrap();
        assert!(
            tracker.id().is_err(),
            "the periodic scan catches a second rollout"
        );
        drop(main_held);
    }

    #[test]
    fn a_codex_switch_before_exit_resumes_the_new_conversation() {
        let home = tempfile::tempdir().unwrap();
        let main = rollout(home.path(), MAIN);
        let other = rollout(home.path(), OTHER);
        let mut tracker = Tracker::new(Provider::Codex, home.path(), None).unwrap();
        let pid = std::process::id();
        let held_main = rollout_writer(&main);
        tracker.observe(pid).unwrap();
        assert_eq!(tracker.id().unwrap(), MAIN);
        drop(held_main);

        let held_other = rollout_writer(&other);
        // /new has switched the writer, but the next periodic scan is far off.
        for _ in 0..3 {
            tracker.observe(pid).unwrap();
        }
        assert_eq!(tracker.known_id(), Some(OTHER));
        drop(held_other);
        tracker.observe_now(pid).unwrap();
        let args = AutoArgs::parse(Provider::Codex, &[]).unwrap();
        let resume = args
            .resume(Provider::Codex, tracker.id().unwrap(), None)
            .unwrap();
        assert_eq!(resume, ["resume", OTHER]);
    }

    #[test]
    fn an_unchanged_codex_writer_can_resume_after_an_idle_exit() {
        let home = tempfile::tempdir().unwrap();
        let main = rollout(home.path(), MAIN);
        // An isolated process keeps unrelated test threads from changing the
        // fd count between observations.
        let mut provider = RolloutProcess::start(&main, &main);
        let mut tracker = Tracker::new(Provider::Codex, home.path(), None).unwrap();
        let pid = provider.child.id();
        tracker.observe(pid).unwrap();
        let scans = FD_SCANS.with(std::cell::Cell::get);
        for _ in 0..3 {
            tracker.observe(pid).unwrap();
        }
        if fd_directory_state(Path::new(&format!("/proc/{pid}/fd"))).is_some() {
            assert_eq!(
                FD_SCANS.with(std::cell::Cell::get),
                scans,
                "a stable tree skips full scans when procfs reports fd counts"
            );
        }

        provider.send("close");
        tracker.observe_now(pid).unwrap();
        let args = AutoArgs::parse(Provider::Codex, &[]).unwrap();
        let resume = args
            .resume(Provider::Codex, tracker.id().unwrap(), None)
            .unwrap();
        assert_eq!(resume, ["resume", MAIN]);
    }

    #[test]
    fn a_retained_codex_writer_does_not_hide_another_writer_until_exit() {
        let home = tempfile::tempdir().unwrap();
        let main = rollout(home.path(), MAIN);
        let other = rollout(home.path(), OTHER);
        let mut provider = RolloutProcess::start(&main, &other);
        let pid = provider.child.id();
        let mut tracker = Tracker::new(Provider::Codex, home.path(), None).unwrap();
        tracker.observe(pid).unwrap();
        assert_eq!(tracker.id().unwrap(), MAIN);

        provider.send("open");
        tracker.observe(pid).unwrap();
        assert!(tracker.id().is_err(), "the live tree contains both writers");

        provider.send("close");
        tracker.observe_now(pid).unwrap();
        assert!(
            tracker.id().is_err(),
            "exit preserves the observed ambiguity"
        );
    }

    #[test]
    fn a_retained_codex_reader_does_not_prove_identity_after_exit() {
        let home = tempfile::tempdir().unwrap();
        let main = rollout(home.path(), MAIN);
        let other = rollout(home.path(), OTHER);
        let mut tracker = Tracker::new(Provider::Codex, home.path(), None).unwrap();
        let pid = std::process::id();
        let reader = fs::File::open(main).unwrap();
        tracker.observe(pid).unwrap();
        assert_eq!(tracker.id().unwrap(), MAIN);

        let writer = rollout_writer(&other);
        for _ in 0..3 {
            tracker.observe(pid).unwrap();
        }
        assert!(tracker.id().is_err(), "the live scan sees both IDs");
        drop((reader, writer));

        // Both descriptors can vanish before the diagnostic forces a scan.
        tracker.observe_now(pid).unwrap();
        assert!(tracker.id().is_err());
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
