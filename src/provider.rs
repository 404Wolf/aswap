use crate::{
    Error, Result, Safe,
    store::{self, Account, Store},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fmt,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(
    Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize, clap::ValueEnum,
)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Claude,
    Codex,
}
impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
    pub fn auth_file(self) -> &'static str {
        match self {
            Self::Claude => ".credentials.json",
            Self::Codex => "auth.json",
        }
    }
    pub fn env_var(self) -> &'static str {
        match self {
            Self::Claude => "CLAUDE_CONFIG_DIR",
            Self::Codex => "CODEX_HOME",
        }
    }
    pub fn current_home(self) -> Result<PathBuf> {
        let p = std::env::var_os(self.env_var())
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or(home()?.join(match self {
                Self::Claude => ".claude",
                Self::Codex => ".codex",
            }));
        store::no_links(&p)?;
        Ok(p)
    }
    pub fn command(self, profile: &Path) -> Command {
        let mut cmd = Command::new(self.as_str());
        cmd.env(self.env_var(), profile);
        // Prevent inherited credentials/config overrides from defeating account isolation.
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
            "ANTHROPIC_BASE_URL",
        ] {
            cmd.env_remove(key);
        }
        if self == Self::Codex {
            cmd.args(["-c", "cli_auth_credentials_store=\"file\""]);
        }
        cmd
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
fn global_config(profile: &Path, default: bool) -> Result<PathBuf> {
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
fn auth_valid(provider: Provider, value: &Value) -> bool {
    let string = |v: Option<&Value>| v.and_then(Value::as_str).is_some_and(|s| !s.is_empty());
    match provider {
        Provider::Claude => string(value.pointer("/claudeAiOauth/accessToken")),
        Provider::Codex => {
            string(value.pointer("/tokens/access_token")) || string(value.get("OPENAI_API_KEY"))
        }
    }
}
pub fn validate_auth(provider: Provider, bytes: &[u8]) -> Result<()> {
    let value: Value =
        serde_json::from_slice(bytes).safe("invalid credential JSON; contents redacted")?;
    if !auth_valid(provider, &value) {
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
    let bytes = store::read_private(&source.join(provider.auth_file()))?;
    validate_auth(provider, &bytes)?;
    let target = store.profile(provider, name)?;
    if target.exists() {
        return Err(Error("profile already exists; refusing to overwrite"));
    }
    store::private_dir(&target)?;
    let result = (|| {
        store::atomic(&target.join(provider.auth_file()), &bytes)?;
        if provider == Provider::Claude {
            let path = global_config(&source, true)?;
            if path.exists() {
                let source: Value = store::json_read(&path)?;
                let mut config = serde_json::json!({"hasCompletedOnboarding":true});
                if let Some(identity) = source.get("oauthAccount") {
                    config["oauthAccount"] = identity.clone();
                }
                store::json_write(&target.join(".claude.json"), &config)?;
            }
        } else {
            store::atomic(
                &target.join("config.toml"),
                b"cli_auth_credentials_store = \"file\"\n",
            )?;
        }
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
    let _lease = store.lease(&account)?;
    if activate {
        activate_profile(store, &account)?;
    }
    state.active.insert(provider, name.to_owned());
    store.save(&state)?;
    Ok(account)
}
pub fn activate_profile(store: &Store, account: &Account) -> Result<()> {
    let provider = account.provider;
    let profile = store.profile(provider, &account.label)?;
    let bytes = store::read_private(&profile.join(provider.auth_file()))?;
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
    let config = if provider == Provider::Claude {
        let path = global_config(&dest, true)?;
        let old: Option<Value> = if path.exists() {
            Some(store::json_read(&path)?)
        } else {
            None
        };
        let mut merged = old.clone().unwrap_or(serde_json::json!({}));
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
pub fn remove(store: &Store, provider: Provider, name: &str) -> Result<()> {
    let _lock = store.lock()?;
    let mut state = store.load()?;
    let account = store.account(&state, provider, Some(name))?;
    let _lease = store.lease(&account)?;
    let profile = store.profile(provider, name)?;
    store::no_links(&profile)?;
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
    Ok(())
}
