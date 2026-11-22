//! Google's Gemini CLI (`gemini`), signed in with a Google account.
//!
//! Verified against the gemini-cli source, v0.47.0 (nixpkgs) and v0.70.0:
//! - Every global path goes through `homedir()`, which is `$GEMINI_CLI_HOME`
//!   when set and `~` otherwise; global state lives in `<home>/.gemini/`
//!   (core/src/utils/paths.ts).
//! - A Google login is `<home>/.gemini/oauth_creds.json`, google-auth-library
//!   `Credentials` (`access_token`, `refresh_token`, `expiry_date` in ms,
//!   `id_token`, ...), written atomically at 0600 and merged on refresh
//!   (code_assist/oauth2.ts). Only `GEMINI_FORCE_ENCRYPTED_FILE_STORAGE=true`
//!   moves it to a keychain; aswap never sets that and scrubs it.
//! - `google_accounts.json` (`{active, old}`) names the signed-in account.
//! - `--session-id <id>` starts a session with that id and `--resume <id>`
//!   resumes it (cli/src/config/config.ts, utils/sessionUtils.ts).
//! - On a limit the CLI prints "Usage limit reached for <model>." and
//!   "You have exhausted your daily quota on this model." Its menu may offer
//!   paid overage or credits; aswap never answers it.
//! - Quota: the CLI's own read-only Code Assist calls `loadCodeAssist` (the
//!   account's project) and `retrieveUserQuota` (per-model buckets).
//!
//! API-key and Vertex logins (`GEMINI_API_KEY`, service accounts) can cost
//! money and are not pooled: capture refuses them.
use crate::{Error, Result, Safe, store};
use serde_json::{Value, json};
use std::{
    os::unix::fs::symlink,
    path::{Path, PathBuf},
};

pub const AUTH_FILE: &str = "oauth_creds.json";
/// The signed-in account's email (`{active, old}`), kept private per view.
pub const ACCOUNTS_FILE: &str = "google_accounts.json";
/// Names the directory that holds `.gemini`, not `.gemini` itself.
pub const HOME_VAR: &str = "GEMINI_CLI_HOME";
/// Inside a launch view: the home Gemini is started with, whose `.gemini`
/// links back to the view, and whose other entries link to the user's home
/// so that `~` in Gemini still means the user's home.
pub const SHIM: &str = ".aswap-home";
/// Inherited variables that would switch Gemini to another login.
pub const SCRUBBED: [&str; 9] = [
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "GOOGLE_APPLICATION_CREDENTIALS",
    "GOOGLE_GENAI_USE_VERTEXAI",
    "GOOGLE_GENAI_USE_GCA",
    "GOOGLE_CLOUD_ACCESS_TOKEN",
    "GEMINI_FORCE_ENCRYPTED_FILE_STORAGE",
    "GEMINI_FORCE_FILE_STORAGE",
    "CODE_ASSIST_ENDPOINT",
];
/// Entries a Gemini launch view always links (created in `~/.gemini` if
/// missing): chats live under `tmp/`, and extensions (hooks such as Herdr's
/// session labeller) under `extensions/`.
pub const SHARED: [&str; 2] = ["tmp", "extensions"];

/// The user's `.gemini`: `$GEMINI_CLI_HOME/.gemini`, else `~/.gemini`.
pub fn current_home() -> Result<PathBuf> {
    let base = std::env::var_os(HOME_VAR)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .map_or_else(crate::provider::home, Ok)?;
    Ok(base.join(".gemini"))
}

/// The `GEMINI_CLI_HOME` for a launch view (or a profile directory).
pub fn cli_home(view: &Path) -> PathBuf {
    view.join(SHIM)
}

/// Lay out `<view>/.aswap-home`: `.gemini` links to the view itself, and
/// every other top-level entry of `user_home` links to the real one.
pub fn prepare_shim(view: &Path, user_home: &Path) -> Result<()> {
    let shim = cli_home(view);
    store::private_dir(&shim)?;
    symlink(view, shim.join(".gemini")).safe("cannot link the Gemini view")?;
    if let Ok(entries) = std::fs::read_dir(user_home) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name == ".gemini" {
                continue;
            }
            let _ = symlink(entry.path(), shim.join(&name));
        }
    }
    Ok(())
}

/// A pooled Gemini login: a Google OAuth login with a refresh token. Not an
/// API key and not a service account (both are billed).
pub fn auth_valid(value: &Value) -> bool {
    let string = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty())
    };
    value.is_object() && value.get("type").is_none() && string("refresh_token")
}

/// `add gemini`: also keep which Google account the login is, for duplicate
/// detection and the view. A missing file is fine.
pub fn capture_extras(source: &Path, target: &Path) -> Result<()> {
    let path = source.join(ACCOUNTS_FILE);
    if path.is_file() {
        let bytes = store::read_private(&path)?;
        serde_json::from_slice::<Value>(&bytes).safe("invalid Gemini account record")?;
        store::atomic(&target.join(ACCOUNTS_FILE), &bytes)?;
    }
    Ok(())
}

/// The account a login belongs to: the id token's subject, else the email
/// `google_accounts.json` names. Never logged.
pub fn account_id(auth: &Value, accounts: &Value) -> Option<String> {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    let claims = auth
        .get("id_token")
        .and_then(Value::as_str)
        .and_then(|t| t.split('.').nth(1))
        .and_then(|p| URL_SAFE_NO_PAD.decode(p).ok())
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .unwrap_or(Value::Null);
    claims
        .get("sub")
        .and_then(Value::as_str)
        .or_else(|| accounts.get("active").and_then(Value::as_str))
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}
/// The signed-in email, for `aswap list` text only.
pub fn email(accounts: &Value) -> Option<String> {
    accounts
        .get("active")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// When the access token expires (Unix seconds).
pub fn expires_at(auth: &Value) -> Option<u64> {
    auth.get("expiry_date")?.as_u64().map(|ms| ms / 1000)
}

/// Google's token endpoint, as google-auth-library uses it.
pub const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

/// Gemini CLI's installed-app OAuth client, which a refresh must present.
/// aswap carries no copy: it reads the pair from the installed Gemini CLI,
/// so it is always the client that CLI uses. Never printed or logged.
pub struct Client {
    id: String,
    secret: String,
}
impl Client {
    /// The installed CLI's client, found once per process.
    pub fn installed() -> Option<&'static Client> {
        static CLIENT: std::sync::OnceLock<Option<Client>> = std::sync::OnceLock::new();
        CLIENT
            .get_or_init(|| {
                let bin = std::env::var_os("ASWAP_GEMINI_BIN")
                    .filter(|s| !s.is_empty())
                    .map(PathBuf::from)
                    .or_else(|| find_on_path("gemini"))?;
                Self::from_install(&bin)
            })
            .as_ref()
    }
    /// The client in the Gemini CLI installed as `bin`: the JavaScript its
    /// launcher runs (Nix: `share/gemini-cli/`, npm: the package's
    /// `bundle/`), searched for the `OAUTH_CLIENT_ID` and
    /// `OAUTH_CLIENT_SECRET` it assigns.
    pub fn from_install(bin: &Path) -> Option<Client> {
        let real = std::fs::canonicalize(bin).ok()?;
        let mut dirs = Vec::new();
        // A launcher script (makeWrapper, npm's shim) names the bundle.
        if let Ok(head) = read_head(&real, 64 * 1024) {
            for word in head.split(|c: char| c.is_whitespace() || c == '"' || c == '\'') {
                if word.starts_with('/')
                    && word.ends_with(".js")
                    && let Some(parent) = Path::new(word).parent()
                {
                    dirs.push(parent.to_path_buf());
                }
            }
        }
        if let Some(prefix) = real.parent().and_then(Path::parent) {
            dirs.push(prefix.join("share/gemini-cli"));
            dirs.push(prefix.join("lib/node_modules/@google/gemini-cli/bundle"));
        }
        if let Some(dir) = real.parent() {
            dirs.push(dir.to_path_buf());
        }
        dirs.iter().find_map(|dir| scan(dir))
    }
}
fn find_on_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}
fn read_head(path: &Path, limit: u64) -> std::io::Result<String> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(limit)
        .read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}
/// At most this many `.js` files of at most this size are searched.
const SCAN_FILES: usize = 400;
const SCAN_BYTES: u64 = 96 * 1024 * 1024;
fn scan(dir: &Path) -> Option<Client> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "js"))
        .collect();
    files.sort();
    files.into_iter().take(SCAN_FILES).find_map(|file| {
        if std::fs::metadata(&file).ok()?.len() > SCAN_BYTES {
            return None;
        }
        let text = std::fs::read(&file).ok()?;
        let text = String::from_utf8_lossy(&text);
        Some(Client {
            id: assigned(&text, "OAUTH_CLIENT_ID", |v| {
                v.ends_with(".apps.googleusercontent.com")
            })?,
            secret: assigned(&text, "OAUTH_CLIENT_SECRET", |v| !v.is_empty())?,
        })
    })
}
/// The string literal assigned to `name` (`name = "…"` or `'…'`), if it
/// passes `valid`.
fn assigned(text: &str, name: &str, valid: impl Fn(&str) -> bool) -> Option<String> {
    text.match_indices(name).find_map(|(at, _)| {
        let rest = text[at + name.len()..].trim_start_matches(|c: char| c.is_ascii_digit());
        let rest = rest.trim_start().strip_prefix('=')?.trim_start();
        let quote = rest.chars().next().filter(|c| matches!(c, '"' | '\''))?;
        let value = &rest[1..rest[1..].find(quote)? + 1];
        (value.len() <= 200
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
            && valid(value))
        .then(|| value.to_owned())
    })
}

/// The form body of a refresh request with the installed CLI's `client`.
pub fn refresh_form(auth: &Value, client: &Client) -> Option<Vec<(&'static str, String)>> {
    let token = auth
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())?;
    Some(vec![
        ("grant_type", "refresh_token".into()),
        ("refresh_token", token.into()),
        ("client_id", client.id.clone()),
        ("client_secret", client.secret.clone()),
    ])
}

/// Merge a token response into the login, as google-auth-library does.
pub fn apply_refresh(old: &Value, response: &Value, at: u64) -> Option<Value> {
    let access = response
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())?;
    let seconds = response
        .get("expires_in")
        .and_then(Value::as_u64)
        .filter(|s| *s > 0)?;
    let mut auth = old.clone();
    let map = auth.as_object_mut()?;
    map.insert("access_token".into(), json!(access));
    map.insert(
        "expiry_date".into(),
        json!(at.saturating_add(seconds).saturating_mul(1000)),
    );
    for key in ["refresh_token", "id_token", "token_type", "scope"] {
        if let Some(value) = response
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            map.insert(key.into(), json!(value));
        }
    }
    Some(auth)
}

/// A view's own login, refreshed by Gemini CLI, that the profile should
/// adopt: the same account, a later expiry, and a refresh token. A refresh
/// token Google rotated this way reaches the profile instead of being lost
/// when the view is rewritten. The caller holds the profile's refresh lock,
/// so one writer at a time updates the profile.
pub fn newer_view_login(view: &Value, profile: &Value) -> Option<Value> {
    if !auth_valid(view) {
        return None;
    }
    let same_account = match (
        account_id(view, &Value::Null),
        account_id(profile, &Value::Null),
    ) {
        (Some(a), Some(b)) => a == b,
        // Without id tokens, the refresh token must match.
        _ => view.get("refresh_token") == profile.get("refresh_token"),
    };
    let newer = expires_at(view).unwrap_or(0) > expires_at(profile).unwrap_or(0);
    (same_account && newer).then(|| {
        let mut merged = profile.clone();
        if let (Some(m), Some(v)) = (merged.as_object_mut(), view.as_object()) {
            for (k, val) in v {
                m.insert(k.clone(), val.clone());
            }
        }
        merged
    })
}

/// Code Assist's endpoint, as Gemini CLI uses it.
const CODE_ASSIST: &str = "https://cloudcode-pa.googleapis.com/v1internal";

/// Usage from the CLI's own read-only quota call: one window per model
/// bucket, `used_percent` from `remainingFraction`, `resets_at` from
/// `resetTime`. A bucket without a fraction is skipped, never invented.
pub fn parse_quota(v: &Value) -> Result<crate::usage::Usage> {
    let buckets = v
        .get("buckets")
        .and_then(Value::as_array)
        .ok_or(Error("unexpected Gemini quota response"))?;
    let mut windows = Vec::new();
    for bucket in buckets {
        let Some(remaining) = bucket.get("remainingFraction").and_then(Value::as_f64) else {
            continue;
        };
        if !(0.0..=1.0).contains(&remaining) {
            return Err(Error("unexpected Gemini quota response"));
        }
        let name = bucket
            .get("modelId")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty() && s.len() <= 64)
            .unwrap_or("quota");
        let resets_at = bucket
            .get("resetTime")
            .and_then(Value::as_str)
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .and_then(|d| u64::try_from(d.timestamp()).ok());
        let used_percent = (1.0 - remaining) * 100.0;
        // A model can have several buckets (requests, tokens): keep the one
        // closest to its limit, so each model is one window.
        if let Some(seen) = windows
            .iter_mut()
            .find(|w: &&mut crate::usage::Window| w.name == name)
        {
            if used_percent > seen.used_percent {
                seen.used_percent = used_percent;
                seen.resets_at = resets_at;
            }
            continue;
        }
        windows.push(crate::usage::Window {
            name: name.to_owned(),
            used_percent,
            resets_at,
            duration_minutes: None,
            codex_slot: None,
        });
    }
    Ok(crate::usage::Usage {
        windows,
        ..Default::default()
    })
}

/// The project `loadCodeAssist` assigns the account, needed by the quota call.
pub fn parse_project(v: &Value) -> Option<String> {
    v.get("cloudaicompanionProject")
        .and_then(|p| {
            p.as_str()
                .map(str::to_owned)
                .or_else(|| p.get("id")?.as_str().map(str::to_owned))
        })
        .filter(|s| !s.is_empty())
}

/// Read quota with the profile's access token. Errors are usage failures;
/// a caller never blocks a launch on them.
pub fn fetch_usage(
    profile: &Path,
) -> std::result::Result<crate::usage::Usage, crate::usage::Failure> {
    use crate::usage::Failure;
    let auth: Value =
        store::json_read(&profile.join(AUTH_FILE)).map_err(|_| Failure::unavailable())?;
    let token = auth
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or(Failure::unavailable())?;
    if expires_at(&auth).is_some_and(|at| at <= crate::now()) {
        return Err(Failure::http(401, 0));
    }
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| Failure::new("network"))?;
    let post = |method: &str, body: Value| -> std::result::Result<Value, Failure> {
        let response = client
            .post(format!("{CODE_ASSIST}:{method}"))
            .bearer_auth(token)
            .json(&body)
            .send()
            .map_err(|_| Failure::new("network"))?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(Failure::http(status, 0));
        }
        let mut bytes = Vec::new();
        use std::io::Read;
        response
            .take(1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| Failure::new("network"))?;
        if bytes.len() > 1024 * 1024 {
            return Err(Failure::unavailable());
        }
        serde_json::from_slice(&bytes).map_err(|_| Failure::unavailable())
    };
    let loaded = post(
        "loadCodeAssist",
        json!({"metadata":{"ideType":"IDE_UNSPECIFIED","platform":"PLATFORM_UNSPECIFIED","pluginType":"GEMINI"}}),
    )?;
    let project = parse_project(&loaded).ok_or(Failure::new("usage-unsupported"))?;
    let quota = post("retrieveUserQuota", json!({"project": project}))?;
    parse_quota(&quota).map_err(|_| Failure::new("usage-unsupported"))
}

/// Gemini's limit notices, normalized (lowercase, straight quotes), both in
/// 0.47.0 and 0.70.0.
pub fn limit_notice(line: &str) -> bool {
    line.strip_prefix("usage limit reached for ")
        .is_some_and(|rest| !rest.is_empty() && rest.len() <= 80)
        || line == "you have exhausted your daily quota on this model."
        || line.starts_with("you have exhausted your daily quota on this model. ")
}

/// Whether `args` already name the session (so aswap mustn't add one).
pub fn names_session(args: &[String]) -> bool {
    args.iter().any(|a| {
        matches!(
            a.as_str(),
            "--session-id" | "--resume" | "-r" | "--session-file"
        ) || a.starts_with("--session-id=")
            || a.starts_with("--resume=")
            || a.starts_with("--session-file=")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn login(expiry_ms: u64, refresh: &str) -> Value {
        json!({"access_token":"synthetic-access","refresh_token":refresh,"expiry_date":expiry_ms,"token_type":"Bearer"})
    }

    #[test]
    fn only_a_google_login_with_a_refresh_token_is_pooled() {
        assert!(auth_valid(&login(1, "synthetic-refresh")));
        assert!(!auth_valid(&login(1, "")));
        assert!(!auth_valid(
            &json!({"type":"service_account","private_key":"x"})
        ));
        assert!(!auth_valid(&json!({"GEMINI_API_KEY":"x"})));
        assert!(!auth_valid(&json!([1, 2])));
    }

    /// A fake Gemini CLI install, laid out as nixpkgs does: a launcher
    /// script naming `share/gemini-cli/gemini.js`, with the client assigned
    /// in a chunk beside it. The values are synthetic.
    pub(crate) fn fake_install(root: &Path) -> PathBuf {
        let share = root.join("share/gemini-cli");
        std::fs::create_dir_all(&share).unwrap();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::write(share.join("gemini.js"), "import './chunk-a.js';\n").unwrap();
        let secret = ["GOCSPX", "-synthetic_secret_value"].concat();
        std::fs::write(
            share.join("chunk-a.js"),
            format!(
                "var x = 1;\nvar OAUTH_CLIENT_ID = \"123456-synthetic.apps.googleusercontent.com\";\nvar OAUTH_CLIENT_SECRET = \"{secret}\";\n"
            ),
        )
        .unwrap();
        let bin = root.join("bin/gemini");
        std::fs::write(
            &bin,
            format!(
                "#!/bin/sh\nexec node --no-warnings=DEP0040 {} \"$@\"\n",
                share.join("gemini.js").display()
            ),
        )
        .unwrap();
        bin
    }

    #[test]
    fn the_client_comes_from_the_installed_cli_and_a_refresh_merges() {
        let t = tempfile::tempdir().unwrap();
        let client = Client::from_install(&fake_install(t.path())).expect("found in the bundle");
        assert_eq!(client.id, "123456-synthetic.apps.googleusercontent.com");
        assert!(client.secret.ends_with("-synthetic_secret_value"));
        // No install, or one without the assignments: no client, no guess.
        assert!(Client::from_install(&t.path().join("missing")).is_none());
        std::fs::write(t.path().join("share/gemini-cli/chunk-a.js"), "var y = 2;").unwrap();
        assert!(Client::from_install(&t.path().join("bin/gemini")).is_none());

        let old = login(1_000, "synthetic-refresh");
        let form = refresh_form(&old, &client).unwrap();
        assert!(form.contains(&("grant_type", "refresh_token".into())));
        assert!(form.contains(&("refresh_token", "synthetic-refresh".into())));
        assert!(form.contains(&("client_id", client.id.clone())));
        let new = apply_refresh(
            &old,
            &json!({"access_token":"synthetic-new","expires_in":3599,"scope":"a b"}),
            100,
        )
        .unwrap();
        assert_eq!(new["access_token"], "synthetic-new");
        assert_eq!(new["expiry_date"], 3_699_000);
        assert_eq!(new["refresh_token"], "synthetic-refresh");
        assert_eq!(new["scope"], "a b");
        assert!(apply_refresh(&old, &json!({"expires_in":1}), 0).is_none());
        assert!(refresh_form(&login(1, ""), &client).is_none());
    }

    /// Run with ASWAP_TEST_REAL_GEMINI=<path to a real bin/gemini> to check
    /// that a real install's client is found. Nothing is printed.
    #[test]
    #[ignore]
    fn a_real_install_has_a_client() {
        let bin = std::env::var_os("ASWAP_TEST_REAL_GEMINI").expect("set ASWAP_TEST_REAL_GEMINI");
        let client = Client::from_install(Path::new(&bin)).expect("client found");
        assert!(client.id.ends_with(".apps.googleusercontent.com"));
        assert!(!client.secret.is_empty());
    }

    #[test]
    fn assignments_are_read_exactly() {
        let text =
            "a.OAUTH_CLIENT_ID2 = 'x-1.apps.googleusercontent.com'; OAUTH_CLIENT_SECRET=\"s_1\"";
        assert_eq!(
            assigned(text, "OAUTH_CLIENT_ID", |v| v
                .ends_with(".apps.googleusercontent.com"))
            .as_deref(),
            Some("x-1.apps.googleusercontent.com")
        );
        assert_eq!(
            assigned(text, "OAUTH_CLIENT_SECRET", |v| !v.is_empty()).as_deref(),
            Some("s_1")
        );
        assert_eq!(
            assigned(
                "OAUTH_CLIENT_SECRET = \"a b\"",
                "OAUTH_CLIENT_SECRET",
                |_| true
            ),
            None
        );
        assert_eq!(
            assigned("OAUTH_CLIENT_SECRET(x)", "OAUTH_CLIENT_SECRET", |_| true),
            None
        );
    }

    #[test]
    fn only_a_newer_login_of_the_same_account_is_written_back() {
        let profile = login(1_000_000, "synthetic-refresh");
        // The CLI refreshed in its view: a later expiry.
        let mut view = login(5_000_000, "synthetic-refresh");
        view["access_token"] = json!("synthetic-fresh");
        let adopted = newer_view_login(&view, &profile).unwrap();
        assert_eq!(adopted["access_token"], "synthetic-fresh");
        // A rotated refresh token reaches the profile with the same account id.
        let id = |sub: &str| {
            use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
            format!(
                "x.{}.y",
                URL_SAFE_NO_PAD.encode(json!({"sub":sub}).to_string())
            )
        };
        let mut p = profile.clone();
        p["id_token"] = json!(id("synthetic-sub"));
        let mut rotated = login(9_000_000, "synthetic-rotated");
        rotated["id_token"] = json!(id("synthetic-sub"));
        assert_eq!(
            newer_view_login(&rotated, &p).unwrap()["refresh_token"],
            "synthetic-rotated"
        );
        // Older, another account, or no refresh token: never.
        assert!(newer_view_login(&profile, &view).is_none());
        let mut other = rotated.clone();
        other["id_token"] = json!(id("synthetic-other"));
        assert!(newer_view_login(&other, &p).is_none());
        assert!(newer_view_login(&login(9_000_000, "synthetic-elsewhere"), &profile).is_none());
        assert!(newer_view_login(&login(9_000_000, ""), &profile).is_none());
    }

    #[test]
    fn quota_buckets_become_windows_and_nothing_is_invented() {
        let usage = parse_quota(&json!({"buckets":[
            {"modelId":"gemini-2.5-pro","remainingFraction":0.25,"resetTime":"2026-11-22T08:00:00Z","tokenType":"REQUESTS"},
            {"modelId":"gemini-2.5-flash","remainingFraction":1.0},
            {"modelId":"gemini-x","remainingAmount":"10"},
            {"modelId":"gemini-2.5-flash","remainingFraction":0.9,"tokenType":"INPUT_TOKENS"}
        ]}))
        .unwrap();
        assert_eq!(usage.windows.len(), 2);
        assert_eq!(usage.windows[0].name, "gemini-2.5-pro");
        assert_eq!(usage.windows[0].used_percent, 75.0);
        assert_eq!(usage.windows[0].resets_at, Some(1_795_334_400));
        assert_eq!(usage.windows[0].duration_minutes, None);
        // The flash model's tighter bucket wins; still one window per model.
        assert!((usage.windows[1].used_percent - 10.0).abs() < 1e-9);
        assert_eq!(usage.windows[1].resets_at, None);
        assert!(parse_quota(&json!({})).is_err());
        assert!(parse_quota(&json!({"buckets":[{"remainingFraction":2.0}]})).is_err());
        assert_eq!(
            parse_project(&json!({"cloudaicompanionProject":"synthetic-project"})).as_deref(),
            Some("synthetic-project")
        );
        assert_eq!(
            parse_project(&json!({"cloudaicompanionProject":{"id":"synthetic-project"}}))
                .as_deref(),
            Some("synthetic-project")
        );
        assert_eq!(parse_project(&json!({})), None);
    }

    #[test]
    fn the_limit_notices_of_both_versions_are_recognized() {
        // 0.47.0 and 0.70.0 (useQuotaAndFallback.ts, googleQuotaErrors.ts).
        for line in [
            "usage limit reached for gemini-2.5-pro.",
            "usage limit reached for all pro models.",
            "you have exhausted your daily quota on this model.",
        ] {
            assert!(limit_notice(line), "{line}");
        }
        for line in [
            "usage limit reached for",
            "the usage limit reached for gemini was a test",
            "possible quota limitations in place or slow response times detected.",
        ] {
            assert!(!limit_notice(line), "{line}");
        }
    }

    #[test]
    fn a_session_named_by_the_user_is_left_alone() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(names_session(&args(&["--resume", "latest"])));
        assert!(names_session(&args(&["--session-id=abc"])));
        assert!(names_session(&args(&["-r"])));
        assert!(!names_session(&args(&["-m", "gemini-2.5-pro"])));
    }

    #[test]
    fn the_shim_points_gemini_at_the_view_and_keeps_tilde_the_users_home() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("home");
        std::fs::create_dir_all(home.join("projects")).unwrap();
        std::fs::create_dir_all(home.join(".gemini")).unwrap();
        let view = t.path().join("view");
        std::fs::create_dir(&view).unwrap();
        prepare_shim(&view, &home).unwrap();
        let shim = cli_home(&view);
        assert_eq!(std::fs::read_link(shim.join(".gemini")).unwrap(), view);
        assert_eq!(
            std::fs::read_link(shim.join("projects")).unwrap(),
            home.join("projects")
        );
    }
}
