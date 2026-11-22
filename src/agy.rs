//! Antigravity CLI (`agy`) accounts: Google sign-ins pooled like Claude and
//! Codex logins. See ~/deacon/work/factory/antigravity/ASWAP-AGY-DESIGN.md
//! and the SPEC it is built on.
//!
//! agy keeps everything under one root (`~/.gemini`, or `--gemini_dir`):
//! its login (`antigravity-cli/antigravity-oauth-token`), settings and
//! conversations. A launch view is a whole root of its own: aswap starts
//! `agy --gemini_dir=<view>` with the view's own `XDG_CONFIG_HOME` and no
//! D-Bus session bus, so the login is always that file, never the shared
//! keyring. Conversations, plugins, hooks, MCP servers and skills are
//! linked in from `~/.gemini`.
//!
//! aswap never calls Google's token endpoint for agy: agy refreshes its own
//! login in place, about hourly, and Google doesn't rotate the refresh
//! token, so copies of one login don't invalidate each other. A view's newer
//! login is written back to its profile (`newer_view_login`). The login in
//! `~/.gemini` itself is only ever read, at `add` time.
use crate::{Error, Result, Safe, store};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// agy's own directory inside a root.
pub const APP: &str = "antigravity-cli";
/// The login, relative to a root (and to a profile).
pub const AUTH_FILE: &str = "antigravity-cli/antigravity-oauth-token";
/// The only sign-in aswap pools: a personal Google account. API keys,
/// application-default credentials and Cloud projects are billed.
const PERSONAL: &str = "consumer";
/// Environment a launch drops: billed sign-ins, and the session bus whose
/// keyring item every account would share.
pub const SCRUBBED: [&str; 9] = [
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "GOOGLE_APPLICATION_CREDENTIALS",
    "GOOGLE_GENAI_USE_VERTEXAI",
    "GOOGLE_CLOUD_PROJECT",
    "GOOGLE_CLOUD_LOCATION",
    "AGY_ADC_AUTH",
    "DBUS_SESSION_BUS_ADDRESS",
    "GEMINI_CLI_HOME",
];
/// Entries of `~/.gemini` linked into every view. The Gemini CLI's own
/// files there (its login above all) are never linked.
pub const SHARED_ROOT: [&str; 4] = ["config", "GEMINI.md", "skills", "antigravity"];
/// Entries of `~/.gemini/antigravity-cli` linked into every view: the
/// conversations and what finds them. `history.jsonl` is a file.
pub const SHARED_APP: [&str; 5] = [
    "brain",
    "conversations",
    "presence",
    "cache",
    "history.jsonl",
];
/// One SQLite file beside the login, so it can't be linked; each view gets
/// a snapshot (the /resume picker's index).
const SUMMARIES: &str = "conversation_summaries.db";

/// How long a limited agy account is left out before a launch may try it
/// again: agy reports when a limit lifts only on its screen.
pub const LIMIT_HOLD: u64 = 3600;

/// For account selection only: an agy reading has no windows, so an
/// account that isn't marked limited is taken as available, with an
/// "assumed" window at 0%. A limited one stays out (its error stands).
pub fn assumed(mut cache: crate::usage::Cached, at: u64) -> crate::usage::Cached {
    if cache.error.is_none() && cache.usage.windows.is_empty() {
        cache.usage.windows.push(crate::usage::Window {
            name: "assumed".into(),
            used_percent: 0.0,
            resets_at: None,
            duration_minutes: None,
            codex_slot: None,
        });
        cache.fetched_at = at;
    }
    cache
}

pub fn current_home() -> Result<PathBuf> {
    Ok(crate::provider::home()?.join(".gemini"))
}

/// A personal Google login agy can refresh by itself.
pub fn auth_valid(auth: &Value) -> bool {
    auth.get("auth_method").and_then(Value::as_str) == Some(PERSONAL)
        && auth
            .pointer("/token/refresh_token")
            .and_then(Value::as_str)
            .is_some_and(|t| !t.is_empty())
}

/// The account's address, as agy recorded it at sign-in.
pub fn email(auth: &Value) -> Option<&str> {
    auth.get("id_token_email")
        .and_then(Value::as_str)
        .filter(|e| !e.is_empty())
}

/// When the access token expires, in Unix seconds (`token.expiry`, RFC 3339).
pub fn expires_at(auth: &Value) -> Option<u64> {
    let expiry = auth.pointer("/token/expiry")?.as_str()?;
    let at = chrono::DateTime::parse_from_rfc3339(expiry).ok()?;
    u64::try_from(at.timestamp()).ok()
}

/// The view's login if agy refreshed it there since the profile's: the same
/// account, a later expiry, still personal and refreshable.
pub fn newer_view_login(view: &Value, profile: &Value) -> Option<Value> {
    (auth_valid(view)
        && email(view).is_some()
        && email(view) == email(profile)
        && expires_at(view)? > expires_at(profile).unwrap_or(0))
    .then(|| view.clone())
}

/// Read a login agy may be writing (it rewrites its token file in place,
/// not by rename): a torn read doesn't parse, so wait for the write to end
/// and read again, then give up.
pub fn read_settled(path: &Path) -> Result<Value> {
    for attempt in 0..3 {
        if let Ok(value) = store::json_read::<Value>(path) {
            return Ok(value);
        }
        if attempt < 2 {
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    }
    Err(Error(
        "the agy login is being written or is not valid JSON; try again in a minute",
    ))
}

/// Lay out a launch view at `view` for the user's `home` (`~/.gemini`).
/// The login and settings are the view's own; everything else agy shares
/// across accounts is linked from `home`, created there first if missing.
pub fn prepare_view(view: &Path, home: &Path) -> Result<()> {
    use std::os::unix::fs::symlink;
    store::private_dir(&home.join(APP))?;
    store::private_dir(&home.join("config"))?;
    for name in SHARED_ROOT {
        let source = home.join(name);
        if source.exists() {
            symlink(&source, view.join(name)).safe("cannot link shared agy state")?;
        }
    }
    let app = view.join(APP);
    store::private_dir(&app)?;
    for name in SHARED_APP {
        let source = home.join(APP).join(name);
        if !source.exists() {
            if name.ends_with(".jsonl") {
                store::atomic(&source, b"")?;
            } else {
                store::private_dir(&source)?;
            }
        }
        symlink(&source, app.join(name)).safe("cannot link shared agy state")?;
    }
    let settings = home.join(APP).join("settings.json");
    if settings.is_file() {
        store::atomic(&app.join("settings.json"), &store::read_private(&settings)?)?;
    }
    let summaries = home.join(APP).join(SUMMARIES);
    if summaries.is_file() {
        // The picker's index; a stale or missing one costs only the picker.
        let _ = crate::shared::sqlite_snapshot(&summaries, &app.join(SUMMARIES));
    }
    Ok(())
}

/// A paid fallback agy offers once quota runs out; aswap never launches
/// with it on.
pub fn spends_credits(view: &Path) -> bool {
    store::json_read::<Value>(&view.join(APP).join("settings.json"))
        .ok()
        .and_then(|s| s.get("useG1Credits").and_then(Value::as_bool))
        == Some(true)
}

/// agy's quota notices, as normalize() leaves them (lowercase, trimmed).
pub fn limit_notice(line: &str) -> bool {
    line.strip_prefix("model ")
        .is_some_and(|rest| rest.contains(" is out of quota until "))
        || line == "quota exhausted"
        || line.starts_with("quota exhausted.")
        || line.starts_with("quota exhausted ·")
        || line == "resource_exhausted"
        || line
            .strip_prefix("error:")
            .is_some_and(|rest| rest.contains("resource_exhausted"))
}

/// Whether `args` already pick a conversation (so aswap mustn't track a
/// new one, and resumes the one agy reports).
pub fn names_session(args: &[String]) -> bool {
    args.iter().any(|a| {
        matches!(a.as_str(), "--conversation" | "-c" | "--continue")
            || a.starts_with("--conversation=")
    })
}

/// The conversation a running agy (`pid`) has open: agy holds
/// `antigravity-cli/presence/<id>.lock` for it.
pub fn conversation_of(pid: u32) -> Option<String> {
    let fds = std::fs::read_dir(format!("/proc/{pid}/fd")).ok()?;
    let mut found = None;
    for fd in fds.flatten() {
        let Ok(target) = std::fs::read_link(fd.path()) else {
            continue;
        };
        let Some(id) = presence_id(&target) else {
            continue;
        };
        if found.as_ref().is_some_and(|f: &String| f != &id) {
            // Two conversations open at once: not one we can name.
            return None;
        }
        found = Some(id);
    }
    found
}

fn presence_id(path: &Path) -> Option<String> {
    let parent = path.parent()?;
    if parent.file_name()? != "presence" || parent.parent()?.file_name()? != APP {
        return None;
    }
    let id = path.file_name()?.to_str()?.strip_suffix(".lock")?;
    crate::session::valid_id(id).then(|| id.to_owned())
}

/// A conversation's transcript under a root.
pub fn transcript(root: &Path, id: &str) -> PathBuf {
    root.join(APP)
        .join("brain")
        .join(id)
        .join(".system_generated/logs/transcript.jsonl")
}

/// aswap processes running another build on this store (`root`): they
/// predate agy support (or a later change), and an older build can't read
/// state.json once it has an agy account. Returns their process ids.
pub fn other_builds(root: &Path) -> Vec<u32> {
    let Ok(me) = std::env::current_exe().and_then(|p| p.canonicalize()) else {
        return Vec::new();
    };
    let Ok(procs) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut others = Vec::new();
    for entry in procs.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(exe) = std::fs::read_link(entry.path().join("exe")) else {
            continue;
        };
        // A replaced binary reads as "<path> (deleted)".
        let name = exe.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !(name == "aswap" || name.starts_with("aswap ")) || exe == me {
            continue;
        }
        if process_store(&entry.path()).as_deref() == Some(root) {
            others.push(pid);
        }
    }
    others
}

/// The store a process uses, from its environment (as Store::root_from_env).
fn process_store(proc_dir: &Path) -> Option<PathBuf> {
    let environ = std::fs::read(proc_dir.join("environ")).ok()?;
    let var = |key: &str| {
        environ.split(|b| *b == 0).find_map(|entry| {
            let entry = std::str::from_utf8(entry).ok()?;
            entry
                .strip_prefix(key)?
                .strip_prefix('=')
                .map(PathBuf::from)
        })
    };
    let base = var("XDG_DATA_HOME")
        .filter(|p| p.is_absolute())
        .or_else(|| var("HOME").map(|home| home.join(".local/share")))?;
    Some(base.join("aswap"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn login(email: &str, expiry: &str) -> Value {
        json!({"token":{"access_token":"synthetic-a","token_type":"Bearer",
            "refresh_token":"synthetic-r","expiry":expiry,"expires_in":3599},
            "auth_method":"consumer","id_token_email":email})
    }

    #[test]
    fn only_a_personal_refreshable_login_is_pooled() {
        assert!(auth_valid(&login(
            "one@example.com",
            "2026-11-22T02:54:54-05:00"
        )));
        let mut billed = login("one@example.com", "2026-11-22T02:54:54-05:00");
        billed["auth_method"] = "api_key".into();
        assert!(!auth_valid(&billed));
        let mut no_refresh = login("one@example.com", "2026-11-22T02:54:54-05:00");
        no_refresh["token"]["refresh_token"] = "".into();
        assert!(!auth_valid(&no_refresh));
        assert!(!auth_valid(&json!({})));
    }

    #[test]
    fn expiry_is_read_with_nanoseconds_and_offset() {
        let auth = login("one@example.com", "2026-11-22T02:54:54.032434564-05:00");
        assert_eq!(expires_at(&auth), Some(1_795_334_094));
        assert_eq!(expires_at(&json!({"token":{"expiry":"soon"}})), None);
    }

    #[test]
    fn a_newer_view_login_of_the_same_account_is_adopted() {
        let profile = login("one@example.com", "2026-11-22T02:00:00Z");
        let refreshed = login("one@example.com", "2026-11-22T03:00:00Z");
        assert!(newer_view_login(&refreshed, &profile).is_some());
        assert!(newer_view_login(&profile, &refreshed).is_none());
        let other = login("two@example.com", "2026-11-22T04:00:00Z");
        assert!(newer_view_login(&other, &profile).is_none());
        let mut signed_out = refreshed.clone();
        signed_out["token"]["refresh_token"] = "".into();
        assert!(newer_view_login(&signed_out, &profile).is_none());
    }

    #[test]
    fn quota_notices_and_prose() {
        for notice in [
            "model gemini-3.1-pro is out of quota until 4:00 pm",
            "quota exhausted",
            "quota exhausted. it resets in 2h",
            "resource_exhausted",
            "error: 429 resource_exhausted",
        ] {
            assert!(limit_notice(notice), "{notice}");
        }
        for prose in [
            "the model is out of quota until tomorrow, they said",
            "what does resource_exhausted mean?",
            "quota exhausted? not yet",
        ] {
            assert!(!limit_notice(prose), "{prose}");
        }
    }

    #[test]
    fn presence_locks_name_the_open_conversation() {
        let id = "666838c2-f624-4ff9-9039-82e50ca540cf";
        let lock = PathBuf::from(format!("/v/antigravity-cli/presence/{id}.lock"));
        assert_eq!(presence_id(&lock).as_deref(), Some(id));
        assert_eq!(presence_id(Path::new("/v/presence/x.lock")), None);
        assert_eq!(
            presence_id(Path::new("/v/antigravity-cli/presence/not-an-id.lock")),
            None
        );
    }

    #[test]
    fn conversation_flags_name_the_session() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(names_session(&args(&["--conversation", "x"])));
        assert!(names_session(&args(&["--conversation=x"])));
        assert!(names_session(&args(&["-c"])));
        assert!(!names_session(&args(&["--model", "m"])));
    }
}
