//! An advisory "newer version available" notice. Nothing is ever updated.
//!
//! Release metadata: the canonical forge (Gitea) is private, and its
//! releases need a login; the public GitHub mirror has no releases, but it
//! carries every release tag (`v<version>`, docs/COMPATIBILITY.md). So the
//! newest release is the highest `v<semver>` tag on the mirror, read
//! anonymously from GitHub's API. Prereleases and other tags are ignored.
//!
//! The check runs at most once a day, in a detached `aswap update-check`
//! process started from a command run on a terminal, so no command waits on
//! the network and scripts never cause a request. It sends no credentials and
//! nothing about the user beyond a plain `aswap/<version>` User-Agent, and
//! any failure (offline, rate limit, a private forge) only backs it off,
//! silently: 1, 2, 4, then at most 7 days. Commands only read the cache.
//!
//! The notice goes to stderr, only from interactive commands (list, status,
//! usage, ...), never with --json and never from run, auto, sync, waybar or
//! gc, so a wrapped agent's terminal or a parser never sees it.
//! `ASWAP_NO_UPDATE_CHECK=1` or `[updates] check = false` in config.toml
//! turns both the check and the notice off.
use crate::{Error, Result, Safe, store};
use semver::Version;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const TAGS_URL: &str = "https://api.github.com/repos/404Wolf/aswap/tags?per_page=100";
pub const TAGS_PAGE: &str = "https://github.com/404Wolf/aswap/tags";
const INTERVAL: u64 = 86_400;
const MAX_BACKOFF: u64 = 7 * 86_400;
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const MAX_BODY: u64 = 1024 * 1024;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cache {
    /// The newest release seen, as `<semver>` without the `v`.
    #[serde(default)]
    pub latest: Option<String>,
    #[serde(default)]
    pub checked_at: u64,
    #[serde(default)]
    pub failures: u32,
    #[serde(default)]
    pub next_check_at: u64,
}

impl Cache {
    pub fn due(&self, now: u64) -> bool {
        now >= self.next_check_at
    }

    /// The cache after a check at `now`: a release on success, `None` on
    /// any failure.
    pub fn after(&self, latest: Option<Version>, now: u64) -> Self {
        match latest {
            Some(latest) => Self {
                latest: Some(latest.to_string()),
                checked_at: now,
                failures: 0,
                next_check_at: now + INTERVAL,
            },
            None => {
                let failures = self.failures.saturating_add(1);
                let backoff = INTERVAL
                    .saturating_mul(1u64 << (failures - 1).min(16))
                    .min(MAX_BACKOFF);
                Self {
                    failures,
                    next_check_at: now + backoff,
                    ..self.clone()
                }
            }
        }
    }
}

/// The highest release tag in a GitHub tags listing.
pub fn latest_release(body: &[u8]) -> Option<Version> {
    #[derive(Deserialize)]
    struct Tag {
        name: String,
    }
    let tags: Vec<Tag> = serde_json::from_slice(body).ok()?;
    tags.iter()
        .filter_map(|t| Version::parse(t.name.strip_prefix('v')?).ok())
        .filter(|v| v.pre.is_empty() && v.build.is_empty())
        .max()
}

/// The notice for `current` given the cache, if a newer release is known.
pub fn notice_for(current: &str, cache: &Cache, nix: bool) -> Option<String> {
    let current = Version::parse(current).ok()?;
    let latest = Version::parse(cache.latest.as_deref()?).ok()?;
    if latest <= current {
        return None;
    }
    let how = if nix {
        "update the aswap input of your Home Manager flake (nix flake update aswap) and switch"
            .to_owned()
    } else {
        format!("see {TAGS_PAGE}")
    };
    Some(format!(
        "aswap: {latest} is available (this is {current}); {how}. \
         ASWAP_NO_UPDATE_CHECK=1 turns this notice off."
    ))
}

fn cache_path() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or(crate::provider::home()?.join(".cache"));
    Ok(base.join("aswap/update-check.json"))
}

fn read(path: &Path) -> Cache {
    store::json_read(path).unwrap_or_default()
}

/// Whether the check and the notice are on: not opted out by environment
/// or by `[updates] check = false` in config.toml.
pub fn enabled() -> bool {
    if std::env::var_os("ASWAP_NO_UPDATE_CHECK").is_some_and(|v| !v.is_empty() && v != "0") {
        return false;
    }
    #[derive(Deserialize, Default)]
    struct Config {
        #[serde(default)]
        updates: Updates,
    }
    #[derive(Deserialize)]
    struct Updates {
        #[serde(default = "yes")]
        check: bool,
    }
    impl Default for Updates {
        fn default() -> Self {
            Self { check: true }
        }
    }
    fn yes() -> bool {
        true
    }
    let Ok(dir) = store::Store::config_dir() else {
        return true;
    };
    let Ok(text) = std::fs::read_to_string(dir.join("config.toml")) else {
        return true;
    };
    toml::from_str::<Config>(&text).map_or(true, |c| c.updates.check)
}

/// The notice if a newer release is cached. On a terminal (`interactive`),
/// a background check starts first when one is due; a script or a test
/// only ever reads the cache. Never blocks, never fails.
pub fn notice(interactive: bool) -> Option<String> {
    if !enabled() {
        return None;
    }
    let path = cache_path().ok()?;
    let cache = read(&path);
    if interactive && cache.due(crate::now()) {
        spawn_check();
    }
    let nix = std::env::current_exe()
        .ok()
        .and_then(|p| p.canonicalize().ok())
        .is_some_and(|p| p.starts_with("/nix/store"));
    notice_for(env!("CARGO_PKG_VERSION"), &cache, nix)
}

fn spawn_check() {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    // Its own process group, so the terminal's ctrl+c never reaches it, and
    // no stdio, so it can't write into the terminal.
    let _ = Command::new(exe)
        .arg("update-check")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn();
}

/// `aswap update-check`: check once if due, and record the result. Only
/// one check runs at a time; a concurrent one returns at once.
pub fn check() -> Result<()> {
    if !enabled() {
        return Ok(());
    }
    let path = cache_path()?;
    let parent = path.parent().ok_or(Error("missing cache directory"))?;
    store::private_dir(parent)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(parent.join("update-check.lock"))
        .safe("cannot open the update-check lock")?;
    if fs2::FileExt::try_lock_exclusive(&lock).is_err() {
        return Ok(());
    }
    let cache = read(&path);
    let now = crate::now();
    if !cache.due(now) {
        return Ok(());
    }
    let url = std::env::var("ASWAP_UPDATE_URL").unwrap_or_else(|_| TAGS_URL.to_owned());
    let latest = fetch(&url).ok().and_then(|body| latest_release(&body));
    store::json_write(&path, &cache.after(latest, crate::now()))
}

fn fetch(url: &str) -> Result<Vec<u8>> {
    use std::io::Read;
    let client = reqwest::blocking::Client::builder()
        .timeout(TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("aswap/", env!("CARGO_PKG_VERSION")))
        .build()
        .safe("cannot build the update-check client")?;
    let response = client
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .safe("update check failed")?;
    if !response.status().is_success() {
        return Err(Error("update check failed"));
    }
    let mut body = Vec::new();
    response
        .take(MAX_BODY + 1)
        .read_to_end(&mut body)
        .safe("update check failed")?;
    if body.len() as u64 > MAX_BODY {
        return Err(Error("update check response too large"));
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_newest_release_tag_wins_and_other_tags_are_ignored() {
        let body = br#"[{"name":"preview/gemini-0f0290a"},{"name":"v0.2.1"},
            {"name":"v0.10.0"},{"name":"v0.11.0-preview.gemini.1"},{"name":"v0.3.0"},
            {"name":"0.99.0"},{"name":"vnext"},{"name":"v1.0.0+build"}]"#;
        assert_eq!(latest_release(body), Some(Version::new(0, 10, 0)));
        assert_eq!(latest_release(br#"[{"name":"preview/x"}]"#), None);
        assert_eq!(latest_release(b"rate limited"), None);
    }

    #[test]
    fn a_notice_only_for_a_strictly_newer_release() {
        let cache = |latest: &str| Cache {
            latest: Some(latest.into()),
            ..Cache::default()
        };
        let newer = notice_for("0.2.1", &cache("0.3.0"), true).unwrap();
        assert!(
            newer.contains("0.3.0 is available (this is 0.2.1)"),
            "{newer}"
        );
        assert!(newer.contains("nix flake update aswap"), "{newer}");
        assert!(newer.contains("ASWAP_NO_UPDATE_CHECK=1"), "{newer}");
        assert!(
            notice_for("0.2.1", &cache("0.3.0"), false)
                .unwrap()
                .contains(TAGS_PAGE)
        );
        assert_eq!(notice_for("0.2.1", &cache("0.2.1"), true), None);
        assert_eq!(notice_for("0.3.0", &cache("0.2.1"), true), None);
        // A prerelease of the next version is newer than the last release.
        assert_eq!(
            notice_for("0.2.2-preview.gemini.1", &cache("0.2.1"), true),
            None
        );
        assert!(notice_for("0.2.2-preview.gemini.1", &cache("0.2.2"), true).is_some());
        assert_eq!(notice_for("0.2.1", &Cache::default(), true), None);
        assert_eq!(notice_for("0.2.1", &cache("garbage"), true), None);
    }

    #[test]
    fn checks_are_daily_and_failures_back_off_to_a_week() {
        let start = Cache::default();
        assert!(start.due(0));
        let ok = start.after(Some(Version::new(0, 3, 0)), 1000);
        assert_eq!(ok.latest.as_deref(), Some("0.3.0"));
        assert_eq!(ok.next_check_at, 1000 + INTERVAL);
        assert!(!ok.due(1000 + INTERVAL - 1) && ok.due(1000 + INTERVAL));
        let mut failed = ok.clone();
        let mut waits = Vec::new();
        for _ in 0..6 {
            failed = failed.after(None, 0);
            waits.push(failed.next_check_at / INTERVAL);
        }
        assert_eq!(waits, [1, 2, 4, 7, 7, 7]);
        // A failure keeps the last known release.
        assert_eq!(failed.latest.as_deref(), Some("0.3.0"));
        assert_eq!(failed.after(Some(Version::new(0, 3, 1)), 0).failures, 0);
    }
}
