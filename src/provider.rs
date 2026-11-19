//! The providers aswap manages. Everything provider-specific (program, home,
//! credentials, refresh, identity, shared-home layout, sessions, limits and
//! usage) lives behind [`Spec`], one implementation per provider in
//! `provider/<name>.rs`; the rest of aswap only asks `provider.spec()`.
mod claude;
mod codex;
mod pi;

use crate::{
    Error, Result, Safe,
    session::SessionId,
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
    Pi,
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
    /// Forks the conversation it names (`--fork <id>`) into a new one.
    ForkFrom,
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
    /// Hand the login in the provider's own home `source` to `keep`, which
    /// saves it into a profile. By default the auth file is copied as it is.
    /// A provider whose own client would go on refreshing that login moves
    /// it instead (two refreshers of one rotating token break each other):
    /// it reads under the provider's lock, runs `keep` under it, and removes
    /// the entry only once `keep` succeeded. If that removal fails it
    /// returns the error, `keep`'s caller rolls back, and the login stays
    /// where it was. A login is never in neither place.
    fn take_login(&self, source: &Path, keep: &mut dyn FnMut(Value) -> Result<()>) -> Result<()> {
        let auth = store::read_private(&source.join(self.auth_file()))?;
        keep(serde_json::from_slice(&auth).safe("invalid credential JSON; contents redacted")?)
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
    fn resume_args(&self, id: &SessionId) -> Vec<String>;
    /// The file where the provider itself records the session that process
    /// `pid` runs. It is read again only when its stamp changes.
    fn session_registry(&self, _home: &Path, _pid: u32) -> Option<PathBuf> {
        None
    }
    /// The session that `registry` (read from `session_registry`) says
    /// process `pid` runs.
    fn registered_session(&self, _registry: &Value, _pid: u32) -> Option<SessionId> {
        None
    }
    /// Whether only the transcript the provider holds open for writing
    /// proves its session (Codex: a retained reader can outlive a switch).
    /// Its descriptors are then rescanned on a cadence, and once more just
    /// before a resume.
    fn session_by_writer(&self) -> bool {
        false
    }
    /// Have a supervised launch report its current session to `report`
    /// (`{"sessionId": ...}`), rewritten whenever the user switches
    /// conversations inside the provider. Returns whether it will.
    fn report_session(&self, _store: &Store, _report: &Path, _cmd: &mut Command) -> Result<bool> {
        Ok(false)
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
    /// Whether a usage-limit notice can come from a login rotated since
    /// launch rather than from the account (Codex): when the account's own
    /// fresh quota has room, the conversation resumes on it with the new
    /// token instead of switching accounts.
    fn limit_from_stale_login(&self) -> bool {
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
    pub const ALL: [Provider; 3] = [Self::Claude, Self::Codex, Self::Pi];
    pub fn spec(self) -> &'static dyn Spec {
        match self {
            Self::Claude => &claude::Claude,
            Self::Codex => &codex::Codex,
            Self::Pi => &pi::Pi,
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
        // Claude Code 2.1.328 stopped passing it to a nested claude; a
        // launch on another account must not inherit it either.
        "CLAUDE_CODE_OAUTH_SCOPES",
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
    add_with(
        store,
        provider,
        name,
        &provider.current_home()?,
        &mut |store, state| store.save(state),
    )
}
/// `add` from the provider home `source`, with the state save injectable so
/// tests can fail it.
fn add_with(
    store: &Store,
    provider: Provider,
    name: &str,
    source: &Path,
    save: &mut dyn FnMut(&Store, &store::State) -> Result<()>,
) -> Result<Account> {
    store::label(name)?;
    let _lock = store.lock()?;
    let state = store.load()?;
    if state
        .accounts
        .iter()
        .any(|a| a.provider == provider && a.label == name)
    {
        return Err(Error("account already exists"));
    }
    let target = store.profile(provider, name)?;
    if target.exists() {
        return Err(Error("profile already exists; refusing to overwrite"));
    }
    let account = Account {
        provider,
        label: name.to_owned(),
    };
    let mut saved = None;
    let result = provider.spec().take_login(source, &mut |login| {
        let bytes = serde_json::to_vec(&login).safe("cannot encode credentials")?;
        validate_auth(provider, &bytes)?;
        store::private_dir(&target)?;
        let path = target.join(provider.auth_file());
        store::atomic(&path, &bytes)?;
        // Verify before anything can take the login out of its source.
        if store::read_private(&path)? != bytes {
            return Err(Error("cannot verify the saved login"));
        }
        provider.spec().capture(source, &target)?;
        let mut next = state.clone();
        next.accounts.push(account.clone());
        next.active
            .entry(provider)
            .or_insert_with(|| name.to_owned());
        save(store, &next)?;
        saved = Some(next);
        Ok(())
    });
    if let Err(err) = result {
        if saved.is_some() {
            // The provider kept the login: forget the account again.
            let _ = store.save(&state);
        }
        let _ = std::fs::remove_dir_all(&target);
        return Err(err);
    }
    Ok(account)
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

#[cfg(test)]
mod scrub_tests {
    use super::*;

    #[test]
    fn a_launch_drops_inherited_claude_oauth_overrides() {
        let mut cmd = Command::new("true");
        scrub(&mut cmd);
        let removed: Vec<_> = cmd
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(key, _)| key.to_owned())
            .collect();
        for key in [
            "CLAUDE_CODE_OAUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_SCOPES",
            "CLAUDE_CODE_OAUTH_CLIENT_ID",
        ] {
            assert!(removed.iter().any(|k| k == key), "{key}");
        }
    }
}

#[cfg(test)]
mod add_tests {
    use super::*;
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;
    fn pi_home(t: &Path, auth: &Value) -> PathBuf {
        let home = t.join("pi-agent");
        std::fs::create_dir_all(&home).unwrap();
        store::json_write(&home.join("auth.json"), auth).unwrap();
        home
    }
    fn login() -> Value {
        // Null optional fields, as pi may write them: kept exactly.
        json!({"type": "oauth", "access": "synthetic-access", "refresh": "synthetic-refresh",
            "expires": 4_000_000_000_000u64, "accountId": null, "clientId": null, "scopes": null})
    }
    fn auth_in(home: &Path) -> Value {
        store::json_read(&home.join("auth.json")).unwrap()
    }
    #[test]
    fn a_moved_pi_login_is_kept_exactly_and_leaves_pi() {
        let t = tempfile::tempdir().unwrap();
        let store = Store::open(t.path().join("store")).unwrap();
        let other = json!({"type": "api_key", "key": "synthetic-key"});
        let home = pi_home(
            t.path(),
            &json!({"anthropic": other, "openai-codex": login()}),
        );
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o755)).unwrap();
        add_with(&store, Provider::Pi, "one", &home, &mut |s, st| s.save(st)).unwrap();
        // pi's directory is pi's: its mode is left alone.
        assert_eq!(
            std::fs::metadata(&home).unwrap().permissions().mode() & 0o777,
            0o755
        );
        let saved: Value = store::json_read(
            &store
                .profile(Provider::Pi, "one")
                .unwrap()
                .join("auth.json"),
        )
        .unwrap();
        assert_eq!(saved, json!({"openai-codex": login()}));
        assert_eq!(auth_in(&home), json!({"anthropic": other}));
        assert!(!home.join("auth.json.lock").exists());
    }
    #[test]
    fn a_failed_save_leaves_the_login_in_pi() {
        let t = tempfile::tempdir().unwrap();
        let store = Store::open(t.path().join("store")).unwrap();
        let before = json!({"openai-codex": login()});
        let home = pi_home(t.path(), &before);
        let err = add_with(&store, Provider::Pi, "one", &home, &mut |_, _| {
            Err(Error("synthetic full disk"))
        })
        .unwrap_err();
        assert_eq!(err.0, "synthetic full disk");
        assert_eq!(auth_in(&home), before, "pi keeps its login");
        assert!(!store.profile(Provider::Pi, "one").unwrap().exists());
        assert!(store.load().unwrap().accounts.is_empty());
        assert!(!home.join("auth.json.lock").exists());
    }
    #[test]
    fn a_failed_removal_from_pi_rolls_the_account_back() {
        let t = tempfile::tempdir().unwrap();
        let store = Store::open(t.path().join("store")).unwrap();
        let before = json!({"openai-codex": login()});
        let home = pi_home(t.path(), &before);
        // The save succeeds, then pi's directory can't take a new file.
        let readonly =
            |mode| std::fs::set_permissions(&home, std::fs::Permissions::from_mode(mode)).unwrap();
        let result = add_with(&store, Provider::Pi, "one", &home, &mut |s, st| {
            s.save(st)?;
            readonly(0o500);
            Ok(())
        });
        readonly(0o700);
        assert!(result.is_err());
        assert_eq!(auth_in(&home), before, "pi keeps its login");
        assert!(!store.profile(Provider::Pi, "one").unwrap().exists());
        let state = store.load().unwrap();
        assert!(state.accounts.is_empty() && state.active.is_empty());
    }
    #[test]
    fn nothing_moves_while_a_live_pi_holds_its_lock() {
        let t = tempfile::tempdir().unwrap();
        let store = Store::open(t.path().join("store")).unwrap();
        let before = json!({"openai-codex": login()});
        let home = pi_home(t.path(), &before);
        std::fs::create_dir(home.join("auth.json.lock")).unwrap();
        let err =
            add_with(&store, Provider::Pi, "one", &home, &mut |s, st| s.save(st)).unwrap_err();
        assert!(err.0.contains("locked by a running pi"));
        assert_eq!(auth_in(&home), before);
        assert!(!store.profile(Provider::Pi, "one").unwrap().exists());
        // The live pi's lock is its own.
        assert!(home.join("auth.json.lock").is_dir());
    }
}
