use crate::{Error, Result, Safe};
use std::{
    collections::{BTreeSet, HashSet},
    ffi::{OsStr, OsString},
    fmt, fs,
    io::{self, ErrorKind},
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};

pub(super) fn lower_priority() -> Result<()> {
    // Linux IOPRIO_WHO_PROCESS and IOPRIO_CLASS_IDLE with data zero.
    // SAFETY: both calls take only integers, with PID zero meaning ourselves.
    let (nice, ionice) = unsafe {
        (
            libc::setpriority(libc::PRIO_PROCESS, 0, 19),
            libc::syscall(libc::SYS_ioprio_set, 1, 0, 3 << 13),
        )
    };
    if nice != 0 || ionice != 0 {
        return Err(Error("cannot lower retention I/O and CPU priority"));
    }
    Ok(())
}

pub(super) fn io_pressure() -> Result<f64> {
    parse_pressure(&fs::read_to_string("/proc/pressure/io").safe("cannot read I/O pressure")?)
}

pub(super) fn parse_pressure(text: &str) -> Result<f64> {
    text.lines()
        .find_map(|line| {
            line.strip_prefix("some ")?
                .split_whitespace()
                .find_map(|field| field.strip_prefix("avg10="))?
                .parse::<f64>()
                .ok()
        })
        .filter(|value| (0.0..=100.0).contains(value))
        .ok_or(Error("cannot read I/O pressure avg10"))
}

pub(super) fn uses_view(view: &Path, ignored: &mut HashSet<OsString>) -> Result<bool> {
    scan(Path::new("/proc"), own_uid(), view, ignored).safe("cannot establish process liveness")
}

fn own_uid() -> u32 {
    // SAFETY: geteuid takes no arguments and does not access memory.
    unsafe { libc::geteuid() }
}

pub(super) fn scan(
    proc_root: &Path,
    uid: u32,
    view: &Path,
    ignored: &mut HashSet<OsString>,
) -> io::Result<bool> {
    scan_with_access(
        proc_root,
        uid,
        view,
        ignored,
        |p| fs::read_link(p),
        |p| fs::read(p),
    )
}

pub(super) fn scan_with_access(
    proc_root: &Path,
    uid: u32,
    view: &Path,
    ignored: &mut HashSet<OsString>,
    cwd: impl Fn(&Path) -> io::Result<PathBuf>,
    environ: impl Fn(&Path) -> io::Result<Vec<u8>>,
) -> io::Result<bool> {
    for entry in fs::read_dir(proc_root)? {
        let entry = entry?;
        let pid = entry.file_name();
        if !pid.as_bytes().iter().all(u8::is_ascii_digit) {
            continue;
        }
        let process = entry.path();
        match process_uses_view(&process, uid, view, &cwd, &environ) {
            Ok(ViewUse::Active) => return Ok(true),
            Ok(ViewUse::Inactive) => {}
            Ok(ViewUse::UnreadableNonProvider) => {
                ignored.insert(pid);
            }
            // A PID can disappear at any point during inspection.
            Err(_) if gone(&process) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(false)
}

enum ViewUse {
    Active,
    Inactive,
    UnreadableNonProvider,
}

fn gone(process: &Path) -> bool {
    matches!(fs::metadata(process), Err(e) if e.kind() == ErrorKind::NotFound)
}

fn may_use_store(process: &Path, uid: u32) -> io::Result<bool> {
    let status = fs::read(process.join("status"))?;
    let field = |prefix: &[u8]| {
        status
            .split(|b| *b == b'\n')
            .find_map(|line| line.strip_prefix(prefix))
    };
    let uid_fields = field(b"Uid:").ok_or_else(|| io::Error::other("missing process UIDs"))?;
    let uids: Vec<u32> = std::str::from_utf8(uid_fields)
        .ok()
        .and_then(|s| {
            s.split_ascii_whitespace()
                .map(|id| id.parse().ok())
                .collect()
        })
        .filter(|uids: &Vec<_>| uids.len() == 4)
        .ok_or_else(|| io::Error::other("invalid process UIDs"))?;
    // Runtimes are private to this Unix user. Other users' process state
    // is ordinarily unreadable. UID transitions involving this user count.
    Ok(uids.contains(&uid)
        && !matches!(
            field(b"State:\t").and_then(|s| s.first()),
            Some(b'Z' | b'X')
        )
        && field(b"Kthread:\t") != Some(b"1"))
}

fn process_uses_view(
    process: &Path,
    uid: u32,
    view: &Path,
    cwd: impl Fn(&Path) -> io::Result<PathBuf>,
    environ: impl Fn(&Path) -> io::Result<Vec<u8>>,
) -> io::Result<ViewUse> {
    if !may_use_store(process, uid)? {
        return Ok(ViewUse::Inactive);
    }
    let cwd = match cwd(&process.join("cwd")) {
        Err(error) if error.kind() == ErrorKind::PermissionDenied => {
            return inspect_unreadable(process, view, error);
        }
        result => result?,
    };
    if cwd.starts_with(view) {
        return Ok(ViewUse::Active);
    }
    let env = match environ(&process.join("environ")) {
        Err(error) if error.kind() == ErrorKind::PermissionDenied => {
            return inspect_unreadable(process, view, error);
        }
        result => result?,
    };
    for value in env.split(|b| *b == 0).filter_map(|var| {
        var.strip_prefix(b"CODEX_HOME=")
            .or_else(|| var.strip_prefix(b"CLAUDE_CONFIG_DIR="))
            .filter(|path| !path.is_empty())
    }) {
        let path = cwd.join(OsStr::from_bytes(value));
        if path.starts_with(view) {
            return Ok(ViewUse::Active);
        }
        // Also recognize a provider home spelled via an alias or .. component.
        match path.canonicalize() {
            Ok(path) if path.starts_with(view) => return Ok(ViewUse::Active),
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(ViewUse::Inactive)
}

fn inspect_unreadable(process: &Path, view: &Path, denied: io::Error) -> io::Result<ViewUse> {
    if fs::read(process.join("cmdline")).is_ok_and(|cmdline| unrelated(&cmdline, view)) {
        return Ok(ViewUse::UnreadableNonProvider);
    }
    Err(denied)
}

fn unrelated(cmdline: &[u8], view: &Path) -> bool {
    let view = view.as_os_str().as_bytes();
    !(cmdline.iter().all(|&b| b == 0)
        || provider_marker(cmdline).is_some()
        || (!view.is_empty() && cmdline.windows(view.len()).any(|part| part == view)))
}

fn provider_marker(cmdline: &[u8]) -> Option<&'static str> {
    ["claude", "codex", "aswap", "node"]
        .into_iter()
        .find(|name| {
            cmdline
                .windows(name.len())
                .any(|part| part.eq_ignore_ascii_case(name.as_bytes()))
        })
}

pub(super) const FD_LIVENESS_ERROR: &str = "cannot establish file descriptor liveness";

#[derive(Debug)]
pub(super) struct FdBlocker {
    pid: OsString,
    excerpt: String,
}

impl fmt::Display for FdBlocker {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            out,
            "PID {}, cmdline excerpt {}",
            self.pid.to_string_lossy(),
            self.excerpt
        )
    }
}

impl From<FdBlocker> for Error {
    fn from(_: FdBlocker) -> Self {
        Error(FD_LIVENESS_ERROR)
    }
}

fn cmdline_excerpt(cmdline: &io::Result<Vec<u8>>) -> String {
    let data = match cmdline {
        Ok(data) if data.iter().any(|&b| b != 0) => data,
        Ok(_) => return "<empty>".into(),
        Err(error) => return format!("<unreadable: {:?}>", error.kind()),
    };
    // Arguments and unfamiliar executable names can contain credentials.
    // Only a recognized basename and fixed provider markers are reportable.
    let argv0 = data
        .split(|&b| matches!(b, 0 | b':' | b' '))
        .next()
        .unwrap_or_default();
    let basename = argv0.rsplit(|&b| b == b'/').next().unwrap_or_default();
    let name = [
        "sshd",
        "sshd-session",
        "ssh-agent",
        "sftp-server",
        "systemd",
        "(sd-pam)",
        "claude",
        "codex",
        "aswap",
        "node",
    ]
    .into_iter()
    .find(|name| basename.eq_ignore_ascii_case(name.as_bytes()))
    .unwrap_or("<argv0 redacted>");
    match provider_marker(data) {
        Some(marker) => format!("{name} (marker {marker})"),
        None => name.into(),
    }
}

struct UnreadableFds {
    pid: OsString,
    path: PathBuf,
    cmdline: io::Result<Vec<u8>>,
}

pub(super) struct Fds {
    paths: BTreeSet<PathBuf>,
    unreadable: Vec<UnreadableFds>,
    uid: u32,
}

impl Fds {
    pub(super) fn for_paths(roots: &[&Path]) -> Result<Self> {
        Self::read(
            Path::new("/proc"),
            own_uid(),
            roots,
            Some(std::process::id()),
        )
        .safe("cannot scan process file descriptors")
    }

    pub(super) fn read(
        proc_root: &Path,
        uid: u32,
        roots: &[&Path],
        skip_pid: Option<u32>,
    ) -> io::Result<Self> {
        Self::read_with_links(proc_root, uid, roots, skip_pid, |path| fs::read_link(path))
    }

    pub(super) fn read_with_links(
        proc_root: &Path,
        uid: u32,
        roots: &[&Path],
        skip_pid: Option<u32>,
        mut read_link: impl FnMut(&Path) -> io::Result<PathBuf>,
    ) -> io::Result<Self> {
        let mut fds = Self {
            paths: BTreeSet::new(),
            unreadable: Vec::new(),
            uid,
        };
        let skip_pid = skip_pid.map(|pid| pid.to_string());
        for entry in fs::read_dir(proc_root)? {
            let entry = entry?;
            let pid = entry.file_name();
            if !pid.as_bytes().iter().all(u8::is_ascii_digit) || pid.to_str() == skip_pid.as_deref()
            {
                continue;
            }
            let process = entry.path();
            match may_use_store(&process, uid) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(_) if gone(&process) => continue,
                Err(error) => return Err(error),
            }
            match fds.read_process(&process, roots, &mut read_link) {
                Ok(()) => {}
                Err(_) if gone(&process) => {}
                Err(error) if error.kind() == ErrorKind::PermissionDenied => {
                    let cmdline = fs::read(process.join("cmdline"));
                    fds.unreadable.push(UnreadableFds {
                        pid,
                        path: process,
                        cmdline,
                    });
                }
                Err(error) => return Err(error),
            }
        }
        Ok(fds)
    }

    fn read_process(
        &mut self,
        process: &Path,
        roots: &[&Path],
        mut read_link: impl FnMut(&Path) -> io::Result<PathBuf>,
    ) -> io::Result<()> {
        for fd in fs::read_dir(process.join("fd"))? {
            let path = match read_link(&fd?.path()) {
                Ok(path) => path,
                Err(error) if error.kind() == ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            let bytes = path.as_os_str().as_bytes();
            let actual = Path::new(OsStr::from_bytes(
                bytes.strip_suffix(b" (deleted)").unwrap_or(bytes),
            ));
            if roots.iter().any(|root| actual.starts_with(root)) {
                self.paths.insert(actual.to_owned());
            }
        }
        Ok(())
    }

    pub(super) fn uses_view(
        &self,
        view: &Path,
        ignored: &mut HashSet<OsString>,
    ) -> std::result::Result<bool, FdBlocker> {
        if self
            .paths
            .range(view.to_owned()..)
            .next()
            .is_some_and(|path| path.starts_with(view))
        {
            return Ok(true);
        }
        for entry in &self.unreadable {
            if entry
                .cmdline
                .as_ref()
                .is_ok_and(|cmdline| unrelated(cmdline, view))
            {
                ignored.insert(entry.pid.clone());
                continue;
            }
            // A process can exit or exec between inspecting status, reading
            // its descriptors and reading cmdline. Do not let an ambiguous
            // observation stay in the batch-wide snapshot indefinitely.
            let cmdline = fs::read(entry.path.join("cmdline"));
            match may_use_store(&entry.path, self.uid) {
                Ok(false) => continue,
                Err(_) if gone(&entry.path) => continue,
                Ok(true)
                    if cmdline
                        .as_ref()
                        .is_ok_and(|cmdline| unrelated(cmdline, view)) =>
                {
                    ignored.insert(entry.pid.clone());
                }
                Ok(true) | Err(_) => {
                    return Err(FdBlocker {
                        pid: entry.pid.clone(),
                        excerpt: cmdline_excerpt(&cmdline),
                    });
                }
            }
        }
        Ok(false)
    }
}
