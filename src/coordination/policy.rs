//! The per-machine sync import allowlist and denylist.
use super::*;

/// Credential-free local import policy, separate from mutable scheduling state.
/// Exact provider:label keys avoid guessing an organization from private identity.
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct SyncPolicy {
    pub(super) allow: Option<Vec<String>>,
    pub(super) deny: Vec<String>,
}
impl SyncPolicy {
    pub(super) fn read(path: &std::path::Path) -> Result<Self> {
        // Declarative configuration may be a Home Manager/Nix symlink. Unlike
        // account-store data this file is never written by aswap.
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // A dangling managed symlink is a broken policy, not no policy.
                return match std::fs::symlink_metadata(path) {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
                    _ => Err(Error("sync: invalid-profile-policy")),
                };
            }
            Err(_) => return Err(Error("sync: invalid-profile-policy")),
        };
        let mut bytes = Vec::new();
        file.take(65537)
            .read_to_end(&mut bytes)
            .map_err(|_| Error("sync: invalid-profile-policy"))?;
        if bytes.len() > 65536 {
            return Err(Error("sync: invalid-profile-policy"));
        }
        let policy: Self =
            serde_json::from_slice(&bytes).map_err(|_| Error("sync: invalid-profile-policy"))?;
        for key in policy.allow.iter().flatten().chain(&policy.deny) {
            let (provider, label) = key
                .split_once(':')
                .ok_or(Error("sync: invalid-profile-policy"))?;
            if !matches!(provider, "claude" | "codex") || store::label(label).is_err() {
                return Err(Error("sync: invalid-profile-policy"));
            }
        }
        Ok(policy)
    }
    pub(super) fn permits(&self, account: &Account) -> bool {
        let key = format!("{}:{}", account.provider.as_str(), account.label);
        self.allow.as_ref().is_none_or(|allow| allow.contains(&key)) && !self.deny.contains(&key)
    }
    pub(super) fn check_local(&self, store: &Store) -> Result<()> {
        if store.load()?.accounts.iter().any(|a| !self.permits(a)) {
            return Err(Error(
                "sync: excluded-local-profile; remove excluded profiles when idle before syncing",
            ));
        }
        Ok(())
    }
}
