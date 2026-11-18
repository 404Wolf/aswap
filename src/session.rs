//! Pass-through auto-mode argv and session identity tracking. Transcript contents are
//! never read: only filenames, metadata, /proc descriptors and PID registry JSON.
use crate::{
    Error, Result, Safe,
    provider::{Provider, SessionArg},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
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
/// A conversation's exact ID: a 36-character UUID, as Claude, Codex and the
/// sessions aswap starts in pi use. Parsed once; never guessed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionId(String);
impl SessionId {
    pub fn parse(id: &str) -> Option<Self> {
        valid_id(id).then(|| Self(id.to_owned()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
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
        let spec = provider.spec();
        if let Some(sub) = args.first().and_then(|s| spec.subcommand(s)) {
            selection = true;
            one_shot = sub.one_shot;
            fork = sub.fork;
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
            let session = spec.session_arg(key);
            if matches!(session, Some(SessionArg::Id | SessionArg::ForkFrom)) {
                fork |= session == Some(SessionArg::ForkFrom);
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
            } else if matches!(session, Some(SessionArg::Continue | SessionArg::Fork)) {
                selection = true;
                fork |= session == Some(SessionArg::Fork);
            } else if session == Some(SessionArg::OneShot) {
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
                ) || spec.takes_value(key);
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
        if let Some(flag) = spec.new_session_flag().filter(|_| !selection) {
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
            initial.splice(at..at, [flag.into(), uuid.clone()]);
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
        let id = SessionId::parse(id).ok_or(Error(
            "exact session identity unavailable; refusing an automatic restart",
        ))?;
        let mut args = provider.spec().resume_args(&id);
        args.extend(self.sticky.clone());
        if let Some(prompt) = prompt {
            args.extend(["--".into(), prompt.into()]);
        }
        Ok(args)
    }
}
#[derive(Clone, PartialEq)]
struct Stamp(u64, u64, i64, i64);
fn files(provider: Provider, home: &Path) -> Result<BTreeMap<PathBuf, Stamp>> {
    fn walk(
        provider: Provider,
        path: &Path,
        depth: usize,
        out: &mut BTreeMap<PathBuf, Stamp>,
    ) -> Result<()> {
        if depth > 5 || !path.is_dir() {
            return Ok(());
        }
        for e in fs::read_dir(path).safe("cannot inspect session filenames")? {
            let e = e.safe("cannot inspect session filename")?;
            let p = e.path();
            let ty = e.file_type().safe("cannot inspect session metadata")?;
            if ty.is_dir() {
                if depth < provider.spec().transcript_depth() {
                    walk(provider, &p, depth + 1, out)?;
                }
            } else if ty.is_file() && filename_id(provider, &p).is_some() {
                let m = e.metadata().safe("cannot inspect session timestamp")?;
                out.insert(p, Stamp(m.ino(), m.len(), m.mtime(), m.mtime_nsec()));
            }
        }
        Ok(())
    }
    let mut out = BTreeMap::new();
    walk(
        provider,
        &home.join(provider.spec().transcripts()),
        0,
        &mut out,
    )?;
    Ok(out)
}
fn filename_id(provider: Provider, path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    if path.extension()? != "jsonl" {
        return None;
    }
    let id = provider.spec().transcript_id(stem)?;
    valid_id(id).then(|| id.to_owned())
}
pub struct Tracker {
    provider: Provider,
    home: PathBuf,
    known: Option<String>,
    files: BTreeMap<PathBuf, Stamp>,
}
impl Tracker {
    pub fn new(provider: Provider, home: &Path, id: Option<String>) -> Result<Self> {
        Ok(Self {
            provider,
            home: home.into(),
            known: id,
            files: files(provider, home)?,
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
        let root = self.home.join(self.provider.spec().transcripts());
        let owned: BTreeSet<_> = descriptors
            .iter()
            .filter(|p| p.starts_with(&root))
            .filter_map(|p| filename_id(self.provider, p))
            .collect();
        let current = files(self.provider, &self.home)?;
        let changed: BTreeSet<_> = current
            .iter()
            .filter(|(p, s)| self.files.get(*p) != Some(*s))
            .filter_map(|(p, _)| filename_id(self.provider, p))
            .collect();
        // A provider's own registry (Claude's PID registry) identifies the
        // main conversation even while subagents hold other transcripts open
        // or unrelated terminals write.
        let registered = self
            .provider
            .spec()
            .registered_session(&self.home, pid)
            .map(|id| id.to_string());
        if let Some(id) = registered {
            self.known = Some(id);
        } else if self.known.is_some()
            && (self.provider.spec().trusts_known_session() || owned.is_empty())
        {
            // Shared history activity cannot contradict a known UUID. In
            // particular, an exited Codex has no descriptors left: another
            // terminal writing then must not strand its exact resume ID.
            // Claude's open files may belong to subagents; Codex can still
            // prove an actual session change with its owned rollout below.
        } else if owned.len() == 1 {
            self.known = owned.into_iter().next();
        } else if owned.len() > 1 || changed.iter().any(|s| self.known.as_ref() != Some(s)) {
            self.known = None;
        }
        self.files = current;
        Ok(())
    }
    /// The tracked conversation's transcript, from the last `observe`
    /// (no walk); None while the identity or the file is unknown.
    pub(crate) fn transcript(&self) -> Option<PathBuf> {
        self.find(self.files.keys())
    }
    /// The same, from a fresh walk: for when no further `observe` will come.
    pub(crate) fn transcript_now(&self) -> Option<PathBuf> {
        self.find(files(self.provider, &self.home).ok()?.keys())
    }
    fn find<'a>(&self, mut paths: impl Iterator<Item = &'a PathBuf>) -> Option<PathBuf> {
        let id = self.known.as_deref()?;
        paths
            .find(|path| filename_id(self.provider, path).as_deref() == Some(id))
            .cloned()
    }
    pub(crate) fn provider(&self) -> Provider {
        self.provider
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
    if let Some(first) = args
        .first()
        .filter(|s| provider.spec().subcommand(s).is_some())
    {
        command.arg(first);
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
    use crate::store;
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
            fs::File::open(other).unwrap(),
        ];
        let pid = std::process::id();
        let registry = home.path().join("sessions").join(format!("{pid}.json"));
        store::json_write(&registry, &serde_json::json!({"pid":pid,"sessionId":OTHER})).unwrap();
        tracker.observe(pid).unwrap();
        assert_eq!(tracker.id().unwrap(), OTHER);
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
        let held = fs::File::open(other).unwrap();
        tracker.observe(std::process::id()).unwrap();
        assert_eq!(tracker.id().unwrap(), OTHER);
        let main = rollout(home.path(), MAIN);
        let _main_held = fs::File::open(main).unwrap();
        tracker.observe(std::process::id()).unwrap();
        assert!(
            tracker.id().is_err(),
            "two owned identities remain ambiguous"
        );
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
