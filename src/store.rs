use crate::provider::Provider;
use crate::{Error, Result, Safe};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Account {
    pub provider: Provider,
    pub label: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Auto {
    #[serde(default = "default_resume_prompt")]
    pub resume_prompt: Option<String>,
    pub enabled: bool,
    pub threshold: f64,
    pub last_switch: u64,
}
pub const RESUME_PROMPT: &str =
    "You were interrupted by a usage-limit account switch. Continue exactly where you left off.";
pub fn default_resume_prompt() -> Option<String> {
    Some(RESUME_PROMPT.into())
}
impl Default for Auto {
    fn default() -> Self {
        Self {
            resume_prompt: default_resume_prompt(),
            enabled: false,
            threshold: 90.0,
            last_switch: 0,
        }
    }
}
#[derive(Default, Serialize, Deserialize)]
pub struct State {
    pub accounts: Vec<Account>,
    pub active: BTreeMap<Provider, String>,
    pub auto: BTreeMap<Provider, Auto>,
}
pub struct Store {
    pub root: PathBuf,
}
pub struct Guard(File);
pub struct Lease {
    _label: Guard,
    _identity: Option<Guard>,
}
impl Drop for Guard {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}

pub fn label(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 48
        || !value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        return Err(Error(
            "labels must be 1–48 ASCII letters, digits, hyphens or underscores",
        ));
    }
    Ok(())
}
/// Reject symlinks at every existing path component, including ancestors.
pub fn no_links(path: &Path) -> Result<()> {
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(Error(
            "an absolute path without parent traversal is required",
        ));
    }
    let mut p = PathBuf::new();
    for part in path.components() {
        p.push(part);
        match fs::symlink_metadata(&p) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(Error("refusing a symlink or managed file"));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(Error("cannot inspect private path")),
        }
    }
    Ok(())
}
pub fn private_dir(path: &Path) -> Result<()> {
    no_links(path)?;
    if !path.exists() {
        if let Some(parent) = path.parent()
            && !parent.exists()
        {
            private_dir(parent)?;
        }
        fs::DirBuilder::new()
            .mode(0o700)
            .create(path)
            .safe("cannot create private directory")?;
    }
    if !path.is_dir() {
        return Err(Error("private directory is not a directory"));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).safe("cannot secure directory")
}
pub fn read_private(path: &Path) -> Result<Vec<u8>> {
    no_links(path)?;
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .safe("cannot read private file")?;
    if !file
        .metadata()
        .safe("cannot inspect private file")?
        .is_file()
    {
        return Err(Error("expected a regular private file"));
    }
    let mut bytes = Vec::new();
    std::io::Read::by_ref(&mut file)
        .take(4 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .safe("cannot read private file")?;
    if bytes.len() > 4 * 1024 * 1024 {
        return Err(Error("private file exceeds size limit"));
    }
    Ok(bytes)
}
pub fn atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    no_links(path)?;
    let parent = path.parent().ok_or(Error("missing private parent"))?;
    private_dir(parent)?;
    let mut temp =
        tempfile::NamedTempFile::new_in(parent).safe("cannot create private temporary file")?;
    temp.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))
        .safe("cannot secure private file")?;
    temp.write_all(bytes).safe("cannot write private file")?;
    temp.as_file().sync_all().safe("cannot sync private file")?;
    temp.persist(path).safe("cannot replace private file")?;
    File::open(parent)
        .and_then(|f| f.sync_all())
        .safe("cannot sync private directory")
}
pub fn json_read<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    serde_json::from_slice(&read_private(path)?).safe("invalid private JSON; contents redacted")
}
pub fn json_write<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    atomic(
        path,
        &serde_json::to_vec_pretty(value).safe("cannot encode private state")?,
    )
}
impl Store {
    pub fn from_env() -> Result<Self> {
        let base = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or(crate::provider::home()?.join(".local/share"));
        Self::open(base.join("aswap"))
    }
    pub fn open(root: PathBuf) -> Result<Self> {
        no_links(&root)?;
        // Public checkout must never become an account store, even by accident.
        if root.ancestors().any(|p| p.join(".git").exists()) {
            return Err(Error("account stores must live outside a Git checkout"));
        }
        private_dir(&root)?;
        Ok(Self { root })
    }
    pub fn lock(&self) -> Result<Guard> {
        let path = self.root.join("store.lock");
        no_links(&path)?;
        let f = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .safe("cannot open store lock")?;
        f.set_permissions(fs::Permissions::from_mode(0o600))
            .safe("cannot secure lock")?;
        f.lock_exclusive().safe("cannot lock account store")?;
        let guard = Guard(f);
        crate::rename::recover(self)?;
        Ok(guard)
    }
    pub fn load(&self) -> Result<State> {
        let p = self.root.join("state.json");
        let state: State = if p.exists() {
            json_read(&p)?
        } else {
            State::default()
        };
        for a in &state.accounts {
            label(&a.label)?;
        }
        for l in state.active.values() {
            label(l)?;
        }
        for a in state.auto.values() {
            threshold(a.threshold)?;
        }
        Ok(state)
    }
    pub fn save(&self, state: &State) -> Result<()> {
        json_write(&self.root.join("state.json"), state)
    }
    pub fn profile(&self, provider: Provider, name: &str) -> Result<PathBuf> {
        label(name)?;
        Ok(self
            .root
            .join("profiles")
            .join(provider.as_str())
            .join(name))
    }
    pub fn sessions(&self, provider: Provider, name: &str) -> Result<PathBuf> {
        label(name)?;
        Ok(self
            .root
            .join("sessions")
            .join(provider.as_str())
            .join(name))
    }
    pub fn account(
        &self,
        state: &State,
        provider: Provider,
        name: Option<&str>,
    ) -> Result<Account> {
        let name = name
            .or_else(|| state.active.get(&provider).map(String::as_str))
            .ok_or(Error("no active account; add or switch first"))?;
        state
            .accounts
            .iter()
            .find(|a| a.provider == provider && a.label == name)
            .cloned()
            .ok_or(Error("account not found"))
    }
    pub fn lease(&self, account: &Account) -> Result<Lease> {
        label(&account.label)?;
        let label = self.lease_label(account)?;
        let identity = crate::identity::read(
            account.provider,
            &self.profile(account.provider, &account.label)?,
        )
        .unwrap_or_default();
        let identity = if let Some(key) = identity.lock_key() {
            Some(self.lease_label(&Account {
                provider: account.provider,
                label: format!("identity-{key}"),
            })?)
        } else {
            None
        };
        Ok(Lease {
            _label: label,
            _identity: identity,
        })
    }
    pub(crate) fn lease_label(&self, account: &Account) -> Result<Guard> {
        let path = self
            .root
            .join(format!("{}-{}.run.lock", account.provider, account.label));
        no_links(&path)?;
        let f = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .safe("cannot open profile lock")?;
        f.try_lock_exclusive()
            .safe("account profile is in use; retry after its session exits")?;
        Ok(Guard(f))
    }
}
pub fn threshold(value: f64) -> Result<()> {
    if !value.is_finite() || !(1.0..=100.0).contains(&value) {
        return Err(Error("threshold must be between 1 and 100"));
    }
    Ok(())
}
