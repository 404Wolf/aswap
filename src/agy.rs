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
use std::{
    io::{Read, Seek},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

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

/// Google's CLI 1.1.11 introduced the zero-turn print-mode quota command.
/// Check the installed binary before passing a slash command as a prompt.
fn supports_headless_quota(version: &[u8]) -> bool {
    let Ok(version) = std::str::from_utf8(version) else {
        return false;
    };
    let mut parts = version.trim().split('.');
    let numbers = [parts.next(), parts.next(), parts.next()]
        .map(|part| part.and_then(|p| p.parse::<u32>().ok()));
    matches!(numbers, [Some(1), Some(minor), Some(patch)] if (minor, patch) >= (1, 11))
        && parts.next().is_none()
}

/// The quota CLI can read using a current access token alone. Keep the
/// refresh token in the profile and all CLI side effects in a disposable root.
pub(crate) fn quota_response(
    profile: &Path,
    at: u64,
    program: Option<&Path>,
) -> std::result::Result<Value, crate::usage::Failure> {
    quota_response_with(profile, at, |root| {
        crate::provider::Provider::Agy.command_with_bin(root, program)
    })
}

fn quota_response_with(
    profile: &Path,
    at: u64,
    make_command: impl Fn(&Path) -> Command,
) -> std::result::Result<Value, crate::usage::Failure> {
    use crate::usage::Failure;

    let login: Value = store::json_read(&profile.join(AUTH_FILE))
        .map_err(|_| Failure::new("agy-usage-auth-unavailable"))?;
    if login.get("auth_method").and_then(Value::as_str) != Some(PERSONAL) {
        return Err(Failure::new("agy-usage-auth-unavailable"));
    }
    let expiry = expires_at(&login).ok_or_else(|| Failure::new("agy-usage-auth-unavailable"))?;
    if expiry <= at {
        return Err(Failure::new("agy-usage-access-expired"));
    }
    let access = login
        .pointer("/token/access_token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| Failure::new("agy-usage-auth-unavailable"))?;
    let mut auth = serde_json::json!({
        "auth_method": PERSONAL,
        "token": {"access_token": access, "expiry": login["token"]["expiry"]},
    });
    if let Some(token_type) = login.pointer("/token/token_type").and_then(Value::as_str) {
        auth["token"]["token_type"] = Value::from(token_type);
    }
    if let Some(email) = email(&login) {
        auth["id_token_email"] = Value::from(email);
    }

    let temporary = tempfile::tempdir().map_err(|_| Failure::new("agy-usage-setup-failed"))?;
    let root = temporary.path();
    let gemini = root.join(".gemini");
    let setup_failed = |_| Failure::new("agy-usage-setup-failed");
    store::json_write(
        &gemini.join(APP).join("settings.json"),
        &serde_json::json!({"useG1Credits": false}),
    )
    .map_err(setup_failed)?;

    let directories = [
        ("XDG_CONFIG_HOME", ".config"),
        ("XDG_DATA_HOME", ".local/share"),
        ("XDG_CACHE_HOME", ".cache"),
        ("XDG_STATE_HOME", ".local/state"),
        ("XDG_RUNTIME_DIR", "run"),
        ("TMPDIR", "tmp"),
    ];
    for (_, name) in directories {
        store::private_dir(&root.join(name)).map_err(setup_failed)?;
    }
    let command = || {
        let mut command = make_command(&gemini);
        let program = command.get_program().to_owned();
        command.env_clear().current_dir(root).env("HOME", root);
        command.env("ASWAP_AGY_BIN", program);
        command.env("AGY_CLI_DISABLE_AUTO_UPDATE", "true");
        command.env("AGY_SYSTEMD_SCOPE", "1");
        for key in ["PATH", "SSL_CERT_FILE", "SSL_CERT_DIR"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        for (key, name) in directories {
            command.env(key, root.join(name));
        }
        command
    };
    let version = quota_output(command().arg("--version"), root)?;
    if !supports_headless_quota(&version) {
        return Err(Failure::new("agy-usage-unsupported-version"));
    }
    store::json_write(&gemini.join(AUTH_FILE), &auth).map_err(setup_failed)?;
    let stdout = quota_output(
        command().args(["-p", "/usage", "--output-format", "json"]),
        root,
    )?;
    serde_json::from_slice(&stdout).map_err(|_| Failure::new("agy-usage-invalid-response"))
}

fn quota_output(
    command: &mut Command,
    root: &Path,
) -> std::result::Result<Vec<u8>, crate::usage::Failure> {
    use crate::usage::Failure;
    const MAX_BYTES: u64 = 256 * 1024;
    let output_failed = |_| Failure::new("agy-usage-output-failed");
    let mut output = tempfile::tempfile_in(root).map_err(output_failed)?;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(output.try_clone().map_err(output_failed)?)
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| Failure::new("agy-usage-start-failed"))?;
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() >= deadline => {
                break Err(Failure::new("agy-usage-timeout"));
            }
            Ok(None) => {}
            Err(_) => break Err(Failure::new("agy-usage-process-failed")),
        }
        match output.metadata() {
            Ok(meta) if meta.len() <= MAX_BYTES => {}
            _ => break Err(Failure::new("agy-usage-output-failed")),
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    if status.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    if !status?.success() {
        return Err(Failure::new("agy-usage-command-failed"));
    }
    output.rewind().map_err(output_failed)?;
    let mut bytes = Vec::new();
    output
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(output_failed)?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(Failure::new("agy-usage-output-failed"));
    }
    Ok(bytes)
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

/// Create a directory of the user's (0700) if it's missing; an existing one
/// is left exactly as it is.
fn ensure_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    if path.exists() {
        return Ok(());
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .safe("cannot create shared agy state")
}

/// Lay out a launch view at `view` for the user's `home` (`~/.gemini`).
/// The login, settings and first-run state are the view's own; everything
/// else agy shares across accounts is linked from `home`, created there
/// first if missing. `config_home` is the user's real `XDG_CONFIG_HOME`: the
/// view's own one links all of it except agy's plugin secrets.
pub fn prepare_view(view: &Path, home: &Path, config_home: &Path) -> Result<()> {
    use std::os::unix::fs::symlink;
    ensure_dir(&home.join(APP))?;
    ensure_dir(&home.join("config"))?;
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
                ensure_dir(&source)?;
            }
        }
        symlink(&source, app.join(name)).safe("cannot link shared agy state")?;
    }
    // Settings and first-run state (onboarding, workspace trust) start as
    // the user's, so a launch doesn't stop at a first-run dialog.
    for name in ["settings.json", "jetski_state.pbtxt"] {
        let source = home.join(APP).join(name);
        if source.is_file() {
            store::atomic(&app.join(name), &store::read_private(&source)?)?;
        }
    }
    let summaries = home.join(APP).join(SUMMARIES);
    if summaries.is_file() {
        // The picker's index; a stale or missing one costs only the picker.
        let _ = crate::shared::sqlite_snapshot(&summaries, &app.join(SUMMARIES));
    }
    config_shim(&view.join(CONFIG), config_home)
}

/// The view's `XDG_CONFIG_HOME`, as agy and every tool it runs see it.
pub const CONFIG: &str = ".config";
/// agy's own entry there: plugin and MCP sign-in secrets, per account.
const CONFIG_OWN: &str = "antigravity";

/// `shim` links every entry of the user's `config_home` (gh, git, herdr,
/// ...) except agy's own, which is the view's. Entries the user adds to
/// `config_home` later appear in the next run's view.
fn config_shim(shim: &Path, config_home: &Path) -> Result<()> {
    use std::os::unix::fs::symlink;
    store::private_dir(shim)?;
    store::private_dir(&shim.join(CONFIG_OWN))?;
    let Ok(entries) = std::fs::read_dir(config_home) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name == CONFIG_OWN {
            continue;
        }
        symlink(entry.path(), shim.join(&name)).safe("cannot link the user's config")?;
    }
    Ok(())
}

/// Remove the login from finished agy views of every account: a run that
/// was killed (SIGKILL) never removed its own. A view still in use holds
/// its lifetime lock and is left alone.
pub fn sweep_finished_views(runtimes: &Path) {
    let Ok(labels) = std::fs::read_dir(runtimes) else {
        return;
    };
    for label in labels.flatten() {
        let Ok(views) = std::fs::read_dir(label.path()) else {
            continue;
        };
        for view in views.flatten() {
            let path = view.path();
            let login = path.join(AUTH_FILE);
            if !login.exists() {
                continue;
            }
            let Ok(lock) = std::fs::OpenOptions::new()
                .read(true)
                .open(path.join(crate::shared::VIEW_LIFETIME_LOCK))
            else {
                continue;
            };
            if fs2::FileExt::try_lock_exclusive(&lock).is_ok() {
                let _ = std::fs::remove_file(&login);
            }
        }
    }
}

/// Options aswap refuses on an agy launch: ones that would move agy off its
/// view (another root or app directory), and session settings that turn on
/// paid credits (`--settings`, inline JSON or a file). Go flags take one or
/// two dashes.
pub fn check_args(args: &[String]) -> Result<()> {
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if arg == "--" {
            break;
        }
        let flag = arg.strip_prefix("--").or_else(|| arg.strip_prefix('-'));
        let Some(flag) = flag.filter(|f| !f.is_empty()) else {
            i += 1;
            continue;
        };
        let (name, inline) = flag
            .split_once('=')
            .map_or((flag, None), |(n, v)| (n, Some(v)));
        match name {
            "gemini_dir" | "app_data_dir" => {
                return Err(Error(
                    "aswap runs agy in its own view; --gemini_dir and --app_data_dir are refused",
                ));
            }
            "settings" => {
                let value = inline
                    .map(str::to_owned)
                    .or_else(|| args.get(i + 1).cloned());
                let settings: Option<Value> = value.as_deref().and_then(|v| {
                    serde_json::from_str(v)
                        .ok()
                        .or_else(|| store::json_read(Path::new(v)).ok())
                });
                let Some(settings) = settings else {
                    return Err(Error(
                        "agy --settings must be inline JSON or a regular JSON file (not a symlink) so aswap can check it",
                    ));
                };
                if settings.get("useG1Credits").and_then(Value::as_bool) == Some(true) {
                    return Err(Error(
                        "agy --settings turns on useG1Credits (paid AI credits); refused",
                    ));
                }
            }
            _ => {}
        }
        i += 1;
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

/// A conversation's transcript under a root.
pub fn transcript(root: &Path, id: &str) -> PathBuf {
    root.join(APP)
        .join("brain")
        .join(id)
        .join(".system_generated/logs/transcript.jsonl")
}

/// aswap processes running another build on this store (`root`): they
/// predate agy support (or a later change), and an older build can't read
/// state.json once it has an agy account. Returns their process ids. It
/// can't stop an older build started later; README says to upgrade every
/// machine's aswap first.
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
        // Only this user's processes can use this user's store; another
        // user's aswap never blocks `add agy`.
        let mine = {
            use std::os::unix::fs::MetadataExt;
            // SAFETY: getuid has no preconditions and cannot fail.
            let uid = unsafe { libc::getuid() };
            std::fs::metadata(entry.path()).is_ok_and(|m| m.uid() == uid)
        };
        if !mine {
            continue;
        }
        let Ok(exe) = std::fs::read_link(entry.path().join("exe")) else {
            continue;
        };
        // A replaced binary reads as "<path> (deleted)".
        let name = exe.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let aswap = name == "aswap"
            || name.starts_with("aswap ")
            || name == ".aswap-wrapped"
            || name.starts_with(".aswap-wrapped ");
        if !aswap || exe == me {
            continue;
        }
        // An environment we can't read could be this store's: count it.
        if process_store(&entry.path()).is_none_or(|store| store == root) {
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
    fn headless_quota_requires_a_supported_stable_version() {
        for version in ["1.1.11\n", "1.5.13\n", "1.5.14\n"] {
            assert!(supports_headless_quota(version.as_bytes()), "{version}");
        }
        for version in ["1.1.10", "1.5.14-dev", "1.5.14.1", "2.0.0", "agy 1.5.14"] {
            assert!(!supports_headless_quota(version.as_bytes()), "{version}");
        }
    }

    #[test]
    fn quota_command_uses_an_isolated_access_only_login() {
        let profile = tempfile::tempdir().unwrap();
        let login_path = profile.path().join(AUTH_FILE);
        let login = login("one@example.com", "2099-01-01T00:00:00Z");
        store::json_write(&login_path, &login).unwrap();
        store::json_write(
            &profile.path().join(APP).join("settings.json"),
            &json!({"useG1Credits": true}),
        )
        .unwrap();
        let response = quota_response_with(profile.path(), 1, |root| {
            let mut command = Command::new("sh");
            command.args([
                "-eu",
                "-c",
                r#"
                test "$PWD/.gemini" = "$1"
                test "$HOME/.gemini" = "$1"
                test "$XDG_CONFIG_HOME" = "$HOME/.config"
                test -z "${DBUS_SESSION_BUS_ADDRESS-}"
                test -z "${GEMINI_API_KEY-}"
                auth="$1/antigravity-cli/antigravity-oauth-token"
                case "$2" in
                --version)
                    test ! -e "$auth"
                    printf '1.5.13\n'
                    ;;
                -p)
                    test "$3 $4 $5" = "/usage --output-format json"
                    grep -q '"access_token": "synthetic-a"' "$auth"
                    if grep -q 'refresh_token' "$auth"; then exit 1; fi
                    grep -q '"useG1Credits": false' "$1/antigravity-cli/settings.json"
                    printf '{"synthetic":true}\n'
                    ;;
                *) exit 1 ;;
                esac
                "#,
                "agy-test",
            ]);
            command.arg(root);
            command
        })
        .unwrap();
        assert_eq!(response, json!({"synthetic": true}));
        assert_eq!(store::json_read::<Value>(&login_path).unwrap(), login);
        let error = quota_response_with(profile.path(), expires_at(&login).unwrap(), |_| {
            panic!("expired access must not start the CLI")
        })
        .unwrap_err();
        assert_eq!(error.kind, "agy-usage-access-expired");
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
    fn a_killed_runs_login_is_swept_and_a_live_ones_kept() {
        let t = tempfile::tempdir().unwrap();
        let runtimes = t.path().join("runtimes/agy");
        let view = |name: &str| {
            let path = runtimes.join("one").join(name);
            std::fs::create_dir_all(path.join(APP)).unwrap();
            std::fs::write(path.join(AUTH_FILE), "{}").unwrap();
            std::fs::write(path.join(crate::shared::VIEW_LIFETIME_LOCK), "").unwrap();
            path
        };
        let killed = view("run-killed");
        let live = view("run-live");
        let held = std::fs::File::open(live.join(crate::shared::VIEW_LIFETIME_LOCK)).unwrap();
        fs2::FileExt::lock_exclusive(&held).unwrap();
        sweep_finished_views(&runtimes);
        assert!(!killed.join(AUTH_FILE).exists());
        assert!(live.join(AUTH_FILE).exists());
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
