//! Pass-through auto-mode argv and session identity tracking. Transcript contents are
//! never read: only filenames, metadata, /proc descriptors and PID registry JSON.
use crate::{Error, Result, Safe, provider::Provider, store};
use serde_json::Value;
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
        let mut one_shot = false;
        if provider == Provider::Codex
            && args
                .first()
                .is_some_and(|s| matches!(s.as_str(), "resume" | "fork" | "exec"))
        {
            selection = true;
            one_shot = args[0] == "exec";
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
                if provider == Provider::Codex || depth == 0 {
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
        &home.join(if provider == Provider::Claude {
            "projects"
        } else {
            "sessions"
        }),
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
    let id = if provider == Provider::Codex {
        stem.get(stem.len().checked_sub(36)?..)?
    } else {
        stem
    };
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
        let root = self.home.join(if self.provider == Provider::Claude {
            "projects"
        } else {
            "sessions"
        });
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
        if owned.len() == 1 {
            self.known = owned.into_iter().next();
        } else if owned.len() > 1 {
            self.known = None;
        } else if self.provider == Provider::Claude {
            let registry = self.home.join("sessions").join(format!("{pid}.json"));
            if let Ok(v) = store::json_read::<Value>(&registry) {
                if v.get("pid").and_then(Value::as_u64) == Some(pid as u64)
                    && let Some(id) = v
                        .get("sessionId")
                        .and_then(Value::as_str)
                        .filter(|s| valid_id(s))
                {
                    if changed.iter().all(|s| s == id) {
                        self.known = Some(id.into());
                    } else {
                        self.known = None;
                    }
                }
            } else if changed.iter().any(|s| self.known.as_ref() != Some(s)) {
                self.known = None;
            }
        }
        // With no PID/descriptor evidence we retain an explicit ID only if no
        // other session changed. A global newest-file heuristic is unsafe.
        else if changed.iter().any(|s| self.known.as_ref() != Some(s)) {
            self.known = None;
        }
        self.files = current;
        Ok(())
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
