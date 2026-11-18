//! The providers aswap manages. Everything provider-specific (program, home,
//! credentials, refresh, identity, shared-home layout, sessions, limits and
//! usage) lives behind [`Spec`], one implementation per provider in
//! `provider/<name>.rs`; the rest of aswap only asks `provider.spec()`.
mod claude;
mod codex;

use crate::{
    Error, Result, Safe,
    store::{self, Account, Store},
    usage::{Failure, Usage},
};
pub use claude::CLIENT as CLAUDE_CLIENT;
pub use codex::CLIENT as CODEX_CLIENT;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fmt,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::Command,
    time::SystemTime,
};

#[derive(
    Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize, clap::ValueEnum,
)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Claude,
    Codex,
}

/// Account hints read from a login, for display and duplicate detection.
#[derive(Default)]
pub struct Claims {
    pub email: Option<String>,
    pub plan: Option<String>,
    /// Stable account identity (hashed before it is compared or stored).
    pub account: Option<String>,
    pub organization: Option<String>,
}
/// How a refresh request is encoded.
pub enum Encoding {
    Json,
    /// `application/x-www-form-urlencoded`; the body is an object of strings.
    Form,
}
/// One OAuth refresh request: where it goes and what it sends.
pub struct Refresh {
    pub url: &'static str,
    pub encoding: Encoding,
    pub body: Value,
}
impl Refresh {
    pub fn json(url: &'static str, body: Value) -> Self {
        Self {
            url,
            encoding: Encoding::Json,
            body,
        }
    }
}
/// What a provider option says about which conversation runs.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SessionArg {
    /// Names a conversation (`--resume <id>`); the value is optional.
    Id,
    /// Picks one by other means (`--continue`, `--last`).
    Continue,
    /// Forks the selected conversation into a new one.
    Fork,
    /// Prints one answer and exits.
    OneShot,
}
/// A leading subcommand that selects or forks a conversation (`codex resume`).
#[derive(Default)]
pub struct Subcommand {
    pub fork: bool,
    pub one_shot: bool,
}

/// Everything aswap needs to know about one provider CLI. Adding a provider is
/// one more implementation of this trait.
pub trait Spec: Sync {
    // The program and its home.
    fn name(&self) -> &'static str;
    /// The default home, relative to `$HOME`.
    fn home_dir(&self) -> &'static str;
    /// The variable that moves the provider's home.
    fn env_var(&self) -> &'static str;
    /// The variable naming the real provider binary (see `Provider::command`).
    fn bin_var(&self) -> &'static str;
    /// The binary to run when `bin_var` is unset.
    fn binary(&self) -> &'static str {
        self.name()
    }
    /// Arguments every launch starts with.
    fn launch_args(&self) -> &'static [&'static str] {
        &[]
    }

    // Credentials: the JSON in `auth_file`.
    fn auth_file(&self) -> &'static str;
    fn auth_valid(&self, auth: &Value) -> bool {
        self.access_token(auth).is_some_and(|s| !s.is_empty())
    }
    fn access_token<'a>(&self, auth: &'a Value) -> Option<&'a str>;
    /// When the access token expires, in Unix seconds.
    fn expires_at(&self, auth: &Value) -> Option<u64>;
    /// Whether this login has a refresh token at all (not an API key).
    fn refreshable(&self, _auth: &Value) -> bool {
        true
    }
    fn refresh(&self, auth: &Value) -> std::result::Result<Refresh, Failure>;
    /// The login after a successful refresh response; unrelated fields stay.
    fn apply_refresh(
        &self,
        old: &Value,
        response: &Value,
        at: u64,
    ) -> std::result::Result<Value, Failure>;
    /// Finish removing the refresh token, after every `refresh_token` and
    /// `refreshToken` key is already gone (for a field the provider requires).
    fn strip_refresh(&self, _auth: &mut Value) {}
    /// An access-only login as a launch view carries it.
    fn launch_auth(&self, auth: Value) -> Value {
        auth
    }
    /// The top-level fields a rotation copies to duplicate labels.
    fn login_fields(&self) -> &'static [&'static str];
    /// Hints from the login and, if the provider has one, its account config.
    fn claims(&self, auth: &Value, config: &Value) -> Claims;
    /// A file in the profile holding account-bound config (Claude's
    /// `.claude.json`), merged into and out of every launch view.
    fn account_config(&self) -> Option<&'static str> {
        None
    }
    /// The login `aswap add` saves from the provider's own auth file.
    fn select_login(&self, auth: Value) -> Value {
        auth
    }
    /// Extra profile files written when a login is added from `source`.
    fn capture(&self, _source: &Path, _profile: &Path) -> Result<()> {
        Ok(())
    }
    /// Whether the outgoing default login `live` is the one saved in `profile`.
    fn same_login(
        &self,
        _live: &Value,
        _live_config: Option<&Value>,
        _saved: &Value,
        _profile: &Path,
    ) -> Result<bool> {
        Ok(false)
    }
    /// Whether `switch --activate-live` may replace the default login file.
    fn activates_live(&self) -> bool {
        false
    }
    /// Whether the auth file also carries the user's MCP server OAuth.
    fn mcp_oauth(&self) -> bool {
        false
    }

    // The shared home.
    /// Directories a launch view always links, created in the home if missing.
    fn shared_dirs(&self) -> &'static [&'static str];
    /// A history file a launch view always links, created if missing.
    fn history_file(&self) -> Option<&'static str> {
        None
    }

    // Sessions.
    /// The home directory holding conversation transcripts.
    fn transcripts(&self) -> &'static str;
    /// How many directory levels below `transcripts` hold transcripts.
    fn transcript_depth(&self) -> usize;
    /// The session ID in a transcript's file stem.
    fn transcript_id<'a>(&self, stem: &'a str) -> Option<&'a str>;
    fn subcommand(&self, _first: &str) -> Option<Subcommand> {
        None
    }
    fn session_arg(&self, _key: &str) -> Option<SessionArg> {
        None
    }
    /// A provider option that always takes one value, beyond the common ones.
    fn takes_value(&self, _key: &str) -> bool {
        false
    }
    /// The option that starts a new conversation with a chosen ID, so a
    /// supervised run knows it from the start.
    fn new_session_flag(&self) -> Option<&'static str> {
        None
    }
    fn resume_args(&self, id: &str) -> Vec<String>;
    /// The session the provider itself says process `pid` runs.
    fn registered_session(&self, _home: &Path, _pid: u32) -> Option<String> {
        None
    }
    /// Whether a known session ID stands until the provider says otherwise:
    /// other writers in the shared home cannot contradict it.
    fn trusts_known_session(&self) -> bool {
        false
    }
    /// Whether usage limits are recorded in the transcript (see `limit_entry`)
    /// rather than only shown on screen.
    fn records_limits(&self) -> bool {
        false
    }
    /// A transcript entry recording a usage limit at or after `since`.
    fn limit_entry(&self, _entry: &Value, _since: SystemTime) -> bool {
        false
    }
    /// Whether accounts can differ in the models they offer (Codex).
    fn model_catalog(&self) -> bool {
        false
    }

    // Usage.
    /// Read the account's quota with the access token in `profile`.
    fn usage(&self, profile: &Path) -> std::result::Result<Usage, Failure>;
}

pub(crate) fn text(v: &Value, paths: &[&str]) -> Option<String> {
    paths.iter().find_map(|p| {
        v.pointer(p)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    })
}
/// The claims of an unverified JWT, for hints and expiry only.
pub(crate) fn jwt_claims(token: &str) -> Option<Value> {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    let bytes = URL_SAFE_NO_PAD.decode(token.split('.').nth(1)?).ok()?;
    serde_json::from_slice(&bytes).ok()
}
fn response_access(response: &Value) -> std::result::Result<&str, Failure> {
    response
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or(Failure::new("refresh-invalid-response"))
}
/// Whether an RFC 3339 `timestamp` (with milliseconds) is at or after `since`.
fn stamped_since(timestamp: Option<&Value>, since: SystemTime) -> bool {
    timestamp
        .and_then(Value::as_str)
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .is_some_and(|at| {
            at >= chrono::DateTime::<chrono::Utc>::from(since - std::time::Duration::from_millis(1))
        })
}

impl Provider {
    pub const ALL: [Provider; 2] = [Self::Claude, Self::Codex];
    pub fn spec(self) -> &'static dyn Spec {
        match self {
            Self::Claude => &claude::Claude,
            Self::Codex => &codex::Codex,
        }
    }
    pub fn as_str(self) -> &'static str {
        self.spec().name()
    }
    pub fn auth_file(self) -> &'static str {
        self.spec().auth_file()
    }
    pub fn env_var(self) -> &'static str {
        self.spec().env_var()
    }
    pub fn current_home(self) -> Result<PathBuf> {
        let p = std::env::var_os(self.env_var())
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or(home()?.join(self.spec().home_dir()));
        store::no_links(&p)?;
        Ok(p)
    }
    /// `ASWAP_CLAUDE_BIN`/`ASWAP_CODEX_BIN` name the real provider binary, so a
    /// `claude` on PATH may itself be a wrapper around `aswap run claude`.
    pub fn bin_var(self) -> &'static str {
        self.spec().bin_var()
    }
    /// The real provider binary: `bin_var` if set, else the plain name.
    pub fn binary(self) -> std::ffi::OsString {
        std::env::var_os(self.bin_var())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| self.spec().binary().into())
    }
    pub fn command(self, profile: &Path) -> Command {
        let bin = self.binary();
        let mut cmd = Command::new(&bin);
        // PATH may contain a provider wrapper that invokes aswap. Tell it this
        // launch is already supervised so it executes the native CLI directly.
        cmd.env(self.bin_var(), &bin);
        cmd.env(self.env_var(), profile);
        scrub(&mut cmd);
        cmd.args(self.spec().launch_args());
        // SAFETY: umask is async-signal-safe; it changes only the forked child.
        unsafe {
            cmd.pre_exec(|| {
                libc::umask(0o077);
                Ok(())
            });
        }
        cmd
    }
}
/// Prevent inherited credentials/config overrides from defeating account isolation.
pub(crate) fn scrub(cmd: &mut Command) {
    for key in [
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR",
        "CLAUDE_SECURESTORAGE_CONFIG_DIR",
        "OPENAI_API_KEY",
        "OPENAI_BASE_URL",
        "CODEX_API_KEY",
        "CODEX_AUTH_JSON",
        "CODEX_REMOTE_TOKEN",
        "CODEX_REMOTE_URL",
        "CODEX_ACCESS_TOKEN",
        "CODEX_REFRESH_TOKEN_URL_OVERRIDE",
        "CODEX_APP_SERVER_LOGIN_CLIENT_ID",
        "CLAUDE_CODE_CUSTOM_OAUTH_URL",
        "CLAUDE_CODE_OAUTH_CLIENT_ID",
        "ANTHROPIC_BASE_URL",
    ] {
        cmd.env_remove(key);
    }
}
impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
pub fn home() -> Result<PathBuf> {
    let p = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or(Error("HOME must be set"))?;
    if !p.is_absolute() {
        return Err(Error("HOME must be absolute"));
    }
    Ok(p)
}
pub(crate) fn global_config(profile: &Path, default: bool) -> Result<PathBuf> {
    let legacy = profile.join(".config.json");
    store::no_links(&legacy)?;
    let global = if default
        && std::env::var_os("CLAUDE_CONFIG_DIR")
            .filter(|s| !s.is_empty())
            .is_none()
    {
        home()?.join(".claude.json")
    } else {
        profile.join(".claude.json")
    };
    if legacy.is_file()
        && (legacy
            .metadata()
            .safe("cannot inspect Claude config")?
            .len()
            > 0
            || !global.exists())
    {
        Ok(legacy)
    } else {
        Ok(global)
    }
}
pub fn validate_auth(provider: Provider, bytes: &[u8]) -> Result<()> {
    let value: Value =
        serde_json::from_slice(bytes).safe("invalid credential JSON; contents redacted")?;
    if !provider.spec().auth_valid(&value) {
        return Err(Error(
            "unsupported or missing credentials; use a file-based provider login first",
        ));
    }
    Ok(())
}
pub fn add(store: &Store, provider: Provider, name: &str) -> Result<Account> {
    store::label(name)?;
    let _lock = store.lock()?;
    let mut state = store.load()?;
    if state
        .accounts
        .iter()
        .any(|a| a.provider == provider && a.label == name)
    {
        return Err(Error("account already exists"));
    }
    let source = provider.current_home()?;
    let home_auth: Value =
        serde_json::from_slice(&store::read_private(&source.join(provider.auth_file()))?)
            .safe("invalid credential JSON; contents redacted")?;
    let bytes = serde_json::to_vec(&provider.spec().select_login(home_auth))
        .safe("cannot encode credentials")?;
    validate_auth(provider, &bytes)?;
    let target = store.profile(provider, name)?;
    if target.exists() {
        return Err(Error("profile already exists; refusing to overwrite"));
    }
    store::private_dir(&target)?;
    let result = (|| {
        store::atomic(&target.join(provider.auth_file()), &bytes)?;
        provider.spec().capture(&source, &target)?;
        let account = Account {
            provider,
            label: name.to_owned(),
        };
        state.accounts.push(account.clone());
        state
            .active
            .entry(provider)
            .or_insert_with(|| name.to_owned());
        store.save(&state)?;
        Ok(account)
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&target);
    }
    result
}
/// Activate a stored login. Never follow Home Manager symlinks or replace unreadable config.
pub fn switch(store: &Store, provider: Provider, name: &str, activate: bool) -> Result<Account> {
    let _lock = store.lock()?;
    let mut state = store.load()?;
    let account = store.account(&state, provider, Some(name))?;
    if activate {
        activate_profile(store, &account)?;
    }
    state.active.insert(provider, name.to_owned());
    store.save(&state)?;
    Ok(account)
}
/// Preserve the outgoing default login, including provider token rotations.
/// Identity matching never guesses from the currently selected label.
fn preserve_live(
    store: &Store,
    target: &Account,
    bytes: &[u8],
    config: Option<&Value>,
) -> Result<bool> {
    validate_auth(target.provider, bytes)?;
    let current: Value =
        serde_json::from_slice(bytes).safe("invalid live credential JSON; contents redacted")?;
    let backups = store
        .root
        .join("live-backups")
        .join(target.provider.as_str());
    store::private_dir(&backups)?;
    let backup = tempfile::Builder::new()
        .prefix("before-switch-")
        .tempdir_in(&backups)
        .safe("cannot preserve outgoing login")?;
    store::atomic(&backup.path().join(target.provider.auth_file()), bytes)?;
    if let Some(config) = config {
        store::json_write(&backup.path().join(".claude.json"), config)?;
    }
    let _ = backup.keep();
    let mut target_is_live = false;
    for candidate in store
        .load()?
        .accounts
        .into_iter()
        .filter(|a| a.provider == target.provider)
    {
        let profile = store.profile(candidate.provider, &candidate.label)?;
        let saved: Value = store::json_read(&profile.join(candidate.provider.auth_file()))?;
        let matched = saved == current
            || target
                .provider
                .spec()
                .same_login(&current, config, &saved, &profile)?;
        if matched {
            let _refresh = store.refresh_lock(&candidate)?;
            store::atomic(&profile.join(candidate.provider.auth_file()), bytes)?;
            target_is_live |= candidate.label == target.label;
        }
    }
    Ok(target_is_live)
}
pub fn activate_profile(store: &Store, account: &Account) -> Result<()> {
    let provider = account.provider;
    if !provider.spec().activates_live() {
        return Err(Error(
            "this provider's default login cannot be swapped; use aswap run",
        ));
    }
    let profile = store.profile(provider, &account.label)?;
    let mut bytes = store::read_private(&profile.join(provider.auth_file()))?;
    validate_auth(provider, &bytes)?;
    let dest = provider.current_home()?;
    let auth_path = dest.join(provider.auth_file());
    store::no_links(&auth_path)?;
    let old_auth = if auth_path.exists() {
        Some(store::read_private(&auth_path)?)
    } else {
        None
    };
    // Construct the merged config before writing either file.
    let mut old_config = None;
    let config = if provider.spec().account_config().is_some() {
        let path = global_config(&dest, true)?;
        let old: Option<Value> = if path.exists() {
            Some(store::json_read(&path)?)
        } else {
            None
        };
        old_config = old.clone();
        let mut merged = old.unwrap_or(serde_json::json!({}));
        if !merged.is_object() {
            return Err(Error("Claude config must be an object; refusing overwrite"));
        }
        let saved_path = profile.join(".claude.json");
        let saved: Value = if saved_path.exists() {
            store::json_read(&saved_path)?
        } else {
            serde_json::json!({})
        };
        if let Some(identity) = saved.get("oauthAccount") {
            merged["oauthAccount"] = identity.clone();
        } else {
            merged.as_object_mut().unwrap().remove("oauthAccount");
        }
        Some((path, merged))
    } else {
        None
    };
    if let Some(old) = &old_auth
        && preserve_live(store, account, old, old_config.as_ref())?
    {
        bytes = old.clone();
    }
    store::atomic(&auth_path, &bytes)?;
    if let Some((path, value)) = config
        && let Err(e) = store::json_write(&path, &value)
    {
        if let Some(old) = old_auth {
            store::atomic(&auth_path, &old)?;
        } else {
            std::fs::remove_file(&auth_path).safe("cannot roll back credential activation")?;
        }
        return Err(e);
    }
    Ok(())
}
pub fn remove(store: &Store, provider: Provider, name: &str) -> Result<Vec<String>> {
    let _lock = store.lock()?;
    let mut state = store.load()?;
    let account = store.account(&state, provider, Some(name))?;
    let _lease = store.lease_label(&account)?;
    let profile = store.profile(provider, name)?;
    store::no_links(&profile)?;
    let duplicates = crate::identity::duplicates(store, &account, &state.accounts);
    // First unlink metadata; a failed deletion leaves an unselectable private orphan.
    state
        .accounts
        .retain(|a| !(a.provider == provider && a.label == name));
    if state.active.get(&provider).is_some_and(|s| s == name) {
        state.active.remove(&provider);
    }
    store.save(&state)?;
    if profile.exists() {
        std::fs::remove_dir_all(profile).safe("cannot remove private profile")?;
    }
    Ok(duplicates)
}
