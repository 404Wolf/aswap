use crate::{Error, Result, Safe};
use std::{
    ffi::OsStr,
    fs,
    io::{self, ErrorKind},
    os::unix::ffi::OsStrExt,
    path::Path,
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

pub(super) fn uses_view(view: &Path) -> Result<bool> {
    // SAFETY: geteuid takes no arguments and does not access memory.
    let uid = unsafe { libc::geteuid() };
    scan(Path::new("/proc"), uid, view).safe("cannot establish process liveness")
}

pub(super) fn scan(proc_root: &Path, uid: u32, view: &Path) -> io::Result<bool> {
    for entry in fs::read_dir(proc_root)? {
        let entry = entry?;
        let pid = entry.file_name();
        if !pid.as_bytes().iter().all(u8::is_ascii_digit) {
            continue;
        }
        let process = entry.path();
        match process_uses_view(&process, uid, view) {
            Ok(true) => return Ok(true),
            Ok(false) => {}
            // A PID can disappear at any point during inspection.
            Err(_) if matches!(fs::metadata(&process), Err(e) if e.kind() == ErrorKind::NotFound) =>
                {}
            Err(error) => return Err(error),
        }
    }
    Ok(false)
}

fn process_uses_view(process: &Path, uid: u32, view: &Path) -> io::Result<bool> {
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
    if !uids.contains(&uid)
        || matches!(
            field(b"State:\t").and_then(|s| s.first()),
            Some(b'Z' | b'X')
        )
        || field(b"Kthread:\t") == Some(b"1")
    {
        return Ok(false);
    }
    let cwd = fs::read_link(process.join("cwd"))?;
    if cwd.starts_with(view) {
        return Ok(true);
    }
    let env = fs::read(process.join("environ"))?;
    for value in env.split(|b| *b == 0).filter_map(|var| {
        var.strip_prefix(b"CODEX_HOME=")
            .or_else(|| var.strip_prefix(b"CLAUDE_CONFIG_DIR="))
            .filter(|path| !path.is_empty())
    }) {
        let path = cwd.join(OsStr::from_bytes(value));
        if path == view {
            return Ok(true);
        }
        // Also recognize a provider home spelled via an alias or .. component.
        match path.canonicalize() {
            Ok(path) if path == view => return Ok(true),
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(false)
}
