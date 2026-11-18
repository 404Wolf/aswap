//! The private, size-capped sync event log.
use super::*;

/// Private, size-capped sync event log: one JSON line per event with the
/// schedule fields. Only fixed application codes, counts and times; never a
/// packet, token, label, email, host or path.
pub(super) const LOG: &str = "sync.log";
pub(super) const LOG_LIMIT: u64 = 1024 * 1024;
pub(super) fn code(error: &Error) -> &'static str {
    let text = error.0.strip_prefix("sync: ").unwrap_or(error.0);
    text.split(';').next().unwrap_or(text)
}
pub(super) fn log(
    store: &Store,
    kind: &str,
    event: &str,
    code: Option<&str>,
    profiles: Option<usize>,
) {
    let at = now();
    let s = settings(store).unwrap_or_default();
    let line = json!({
        "at": at,
        "pid": std::process::id(),
        "kind": kind,
        "event": event,
        "code": code,
        "profiles": profiles,
        "last_sync": s.last_sync,
        "last_attempt": s.last_attempt,
        "failures": s.failures,
        "next_poll": next_poll(&s, at),
        "watch_next": s.watch_next,
        "watch_failures": s.watch_failures,
    });
    let _ = append_log(&store.root.join(LOG), &line);
}
pub(super) fn append_log(path: &std::path::Path, line: &Value) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    store::no_links(path)?;
    if std::fs::metadata(path).is_ok_and(|m| m.len() >= LOG_LIMIT) {
        let old = path.with_extension("log.1");
        store::no_links(&old)?;
        std::fs::rename(path, &old).safe("cannot rotate sync log")?;
    }
    let mut bytes = serde_json::to_vec(line).safe("cannot encode sync log")?;
    bytes.push(b'\n');
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .safe("cannot open sync log")?;
    // One write per line: concurrent appenders never interleave a line.
    file.write_all(&bytes).safe("cannot write sync log")
}
/// The newest `count` sync events, oldest first, for `status --json`.
pub fn recent_events(store: &Store, count: usize) -> Vec<Value> {
    let Ok(bytes) = store::read_private(&store.root.join(LOG)) else {
        return Vec::new();
    };
    let mut events: Vec<Value> = bytes
        .split(|b| *b == b'\n')
        .rev()
        .filter_map(|line| serde_json::from_slice(line).ok())
        .take(count)
        .collect();
    events.reverse();
    events
}
