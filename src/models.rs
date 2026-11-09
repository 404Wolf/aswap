//! Per-run model eligibility. Never infer access from an account label or tier.
use crate::{
    Error, Result, Safe, now,
    provider::Provider,
    store::{self, Account, Store},
    usage::{self, Cached},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

#[derive(Default)]
pub struct Gate {
    pub required: Option<String>,
    pub pin: bool,
    home: PathBuf,
    overrides: Vec<String>,
    cwd: PathBuf,
    profile: Option<String>,
    // Access token fingerprint prevents reusing a catalog after login replacement.
    checked: BTreeMap<String, (Vec<u8>, u64, bool)>,
    rejected: BTreeSet<String>,
}
fn request(home: &Path, overrides: &[String], method: &str, params: Value) -> Result<Value> {
    usage::codex_request(
        home,
        &std::cell::Cell::new(usage::Failure::new("codex-rpc-unavailable")),
        overrides,
        method,
        params,
    )
}
fn model_name(value: &str) -> Result<String> {
    // A model is a provider identifier, never terminal control text or a prompt.
    if value.is_empty()
        || value.len() > 256
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_/.:".contains(&b))
    {
        return Err(Error(
            "invalid model identifier for automatic account selection",
        ));
    }
    Ok(value.into())
}
fn merge_profile(base: &mut toml::Value, layer: toml::Value) {
    if let (Some(base), Some(layer)) = (base.as_table_mut(), layer.as_table()) {
        for (key, value) in layer {
            if let Some(before) = base.get_mut(key) {
                merge_profile(before, value.clone());
            } else {
                base.insert(key.clone(), value.clone());
            }
        }
    } else {
        *base = layer;
    }
}
fn option(arg: &str) -> (&str, Option<&str>) {
    if arg.len() > 2
        && ["-m", "-c", "-p", "-C"]
            .iter()
            .any(|flag| arg.starts_with(flag))
    {
        let value = &arg[2..];
        return (&arg[..2], Some(value.strip_prefix('=').unwrap_or(value)));
    }
    arg.split_once('=')
        .map_or((arg, None), |(key, value)| (key, Some(value)))
}
impl Gate {
    pub fn new(provider: Provider, args: &[String]) -> Result<Self> {
        if provider != Provider::Codex {
            return Ok(Self::default());
        }
        let mut gate = Self {
            home: Provider::Codex.current_home()?,
            cwd: std::env::current_dir().safe("cannot resolve model config directory")?,
            ..Self::default()
        };
        let mut explicit = None;
        let mut i = 0;
        while i < args.len() {
            let (key, inline) = option(&args[i]);
            if key == "--" {
                break;
            }
            if matches!(
                key,
                "-m" | "--model" | "-c" | "--config" | "-p" | "--profile" | "-C" | "--cd"
            ) {
                let value = if let Some(v) = inline {
                    v
                } else {
                    i += 1;
                    args.get(i)
                        .ok_or(Error("missing model/config option value"))?
                };
                match key {
                    "-m" | "--model" => explicit = Some(model_name(value)?),
                    "-c" | "--config" => {
                        gate.overrides.extend(["-c".into(), value.into()]);
                    }
                    "-p" | "--profile" => {
                        store::label(value)?;
                        gate.profile = Some(value.into());
                    }
                    _ => gate.cwd = gate.cwd.join(value),
                }
            }
            i += 1;
        }
        // Explicit -m wins over config. Otherwise ask Codex to resolve config
        // layers/profiles/overrides itself, rather than implementing TOML rules.
        gate.pin = explicit.is_none();
        gate.required = explicit;
        Ok(gate)
    }
    fn auth(store: &Store, a: &Account) -> Result<Value> {
        store::json_read(
            &store
                .profile(a.provider, &a.label)?
                .join(a.provider.auth_file()),
        )
    }
    fn fingerprint(auth: &Value) -> Vec<u8> {
        ring::digest::digest(&ring::digest::SHA256, auth.to_string().as_bytes())
            .as_ref()
            .to_vec()
    }
    fn probe_home(&self, store: &Store, a: &Account) -> Result<tempfile::TempDir> {
        let temporary = tempfile::tempdir().safe("cannot prepare model eligibility check")?;
        let auth = Self::auth(store, a)?;
        store::json_write(
            &temporary.path().join(a.provider.auth_file()),
            &crate::refresh::access_only(a.provider, &auth),
        )?;
        let config = self.home.join("config.toml");
        if config.exists() || self.profile.is_some() {
            let mut bytes = if config.exists() {
                std::fs::read(config).safe("cannot read model configuration")?
            } else {
                Vec::new()
            };
            if let Some(profile) = &self.profile {
                let layer =
                    std::fs::read_to_string(self.home.join(format!("{profile}.config.toml")))
                        .safe("cannot read selected Codex model profile")?;
                let mut base: toml::Value = toml::from_str(
                    std::str::from_utf8(&bytes).safe("invalid model configuration")?,
                )
                .safe("invalid model configuration")?;
                let layer: toml::Value =
                    toml::from_str(&layer).safe("invalid Codex model profile")?;
                merge_profile(&mut base, layer);
                bytes = toml::to_string(&base)
                    .safe("cannot prepare model profile")?
                    .into_bytes();
            }
            store::atomic(&temporary.path().join("config.toml"), &bytes)?;
        }
        Ok(temporary)
    }
    fn catalog(&self, home: &Path) -> Result<Vec<Value>> {
        let mut rows = Vec::new();
        let mut cursor = Value::Null;
        for _ in 0..16 {
            let result = request(
                home,
                &self.overrides,
                "model/list",
                json!({"includeHidden":true,"limit":100,"cursor":cursor}),
            )?;
            rows.extend(
                result["data"]
                    .as_array()
                    .ok_or(Error(
                        "invalid model catalog; automatic account switch refused",
                    ))?
                    .iter()
                    .cloned(),
            );
            let next = result.get("nextCursor").cloned().unwrap_or(Value::Null);
            if next.is_null() {
                return Ok(rows);
            }
            if next == cursor || !next.is_string() {
                break;
            }
            cursor = next;
        }
        Err(Error(
            "model catalog pagination incomplete; automatic account switch refused",
        ))
    }
    pub fn resolve(&mut self, store: &Store, current: &Account) -> Result<()> {
        if current.provider != Provider::Codex || self.required.is_some() {
            return Ok(());
        }
        let home = self.probe_home(store, current)?;
        let config = request(
            home.path(),
            &self.overrides,
            "config/read",
            json!({"includeLayers":false,"cwd":self.cwd}),
        ).map_err(|_| Error("cannot resolve session model from Codex config; specify --model for automatic account switching"))?;
        let model = if let Some(m) = config.pointer("/config/model").and_then(Value::as_str) {
            m.to_owned()
        } else {
            self.catalog(home.path())?
                .into_iter()
                .find(|m| m["isDefault"] == true)
                .and_then(|m| m["model"].as_str().map(str::to_owned))
                .ok_or(Error(
                    "cannot determine session model; specify --model before automatic switching",
                ))?
        };
        self.required = Some(model_name(&model)?);
        Ok(())
    }
    pub fn reject(&mut self, account: &Account) {
        self.rejected.insert(account.label.clone());
    }
    pub fn allows_cached(&self, store: &Store, account: &Account) -> bool {
        if self.required.is_none() {
            return true;
        }
        if self.rejected.contains(&account.label) {
            return false;
        }
        let Ok(auth) = Self::auth(store, account) else {
            return false;
        };
        self.checked
            .get(&account.label)
            .is_some_and(|(fingerprint, at, supported)| {
                *supported
                    && now().saturating_sub(*at) < 300
                    && *fingerprint == Self::fingerprint(&auth)
            })
    }
    fn allows(&mut self, store: &Store, account: &Account) -> bool {
        let Some(model) = self.required.clone() else {
            return true;
        };
        if self.rejected.contains(&account.label) {
            return false;
        }
        let Ok(auth) = Self::auth(store, account) else {
            return false;
        };
        let fingerprint = Self::fingerprint(&auth);
        if let Some((before, at, supported)) = self.checked.get(&account.label)
            && *before == fingerprint
            && now().saturating_sub(*at) < 300
        {
            return *supported;
        }
        let supported = self
            .probe_home(store, account)
            .and_then(|home| self.catalog(home.path()))
            .is_ok_and(|rows| {
                rows.iter()
                    .any(|row| row["model"] == model || row["id"] == model)
            });
        self.checked
            .insert(account.label.clone(), (fingerprint, now(), supported));
        if !supported {
            eprintln!(
                "aswap: skipping codex:{}: model {model} is unavailable or could not be verified",
                account.label
            );
        }
        supported
    }
    /// Keep current quota for threshold decisions, but deny unsupported targets.
    pub fn filter(
        &mut self,
        store: &Store,
        current: &str,
        mut entries: Vec<(Account, Cached)>,
    ) -> Vec<(Account, Cached)> {
        for (account, cache) in &mut entries {
            if account.label != current
                && cache.used(now()).is_some()
                && !self.allows(store, account)
            {
                cache.error = Some("model-unavailable".into());
            }
        }
        entries
    }
    pub fn filter_cached(
        &self,
        store: &Store,
        current: &str,
        mut entries: Vec<(Account, Cached)>,
    ) -> Vec<(Account, Cached)> {
        for (account, cache) in &mut entries {
            if account.label != current && !self.allows_cached(store, account) {
                cache.error = Some("model-unavailable".into());
            }
        }
        entries
    }
    pub fn report_none(&self) {
        if let Some(model) = &self.required {
            eprintln!(
                "aswap: no eligible account with verified support for model {model} and available quota; keeping the session model unchanged"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn profile_overlay_preserves_base_and_overrides_nested_values() {
        let mut base: toml::Value =
            toml::from_str("model='base'\n[features]\na=true\nb=false").unwrap();
        let layer: toml::Value = toml::from_str("model='profile'\n[features]\nb=true").unwrap();
        merge_profile(&mut base, layer);
        assert_eq!(base["model"].as_str(), Some("profile"));
        assert_eq!(base["features"]["a"].as_bool(), Some(true));
        assert_eq!(base["features"]["b"].as_bool(), Some(true));
    }
    #[test]
    fn credential_replacement_invalidates_model_evidence() {
        let t = tempfile::tempdir().unwrap();
        let store = Store::open(t.path().join("store")).unwrap();
        let a = Account {
            provider: Provider::Codex,
            label: "synthetic".into(),
        };
        let path = store
            .profile(a.provider, &a.label)
            .unwrap()
            .join("auth.json");
        let original = json!({"tokens":{"access_token":"synthetic-one"}});
        store::json_write(&path, &original).unwrap();
        let mut gate = Gate {
            required: Some("synthetic-sol".into()),
            ..Gate::default()
        };
        gate.checked
            .insert(a.label.clone(), (Gate::fingerprint(&original), now(), true));
        assert!(gate.allows_cached(&store, &a));
        store::json_write(&path, &json!({"tokens":{"access_token":"synthetic-two"}})).unwrap();
        assert!(!gate.allows_cached(&store, &a));
    }
    #[test]
    fn option_values_cover_joined_short_flags_and_inline_long_flags() {
        assert_eq!(option("-msynthetic-sol"), ("-m", Some("synthetic-sol")));
        assert_eq!(
            option("--model=synthetic-sol"),
            ("--model", Some("synthetic-sol"))
        );
        assert_eq!(
            option("-cmodel=\"synthetic-sol\""),
            ("-c", Some("model=\"synthetic-sol\""))
        );
    }
    #[test]
    fn model_identifiers_cannot_inject_terminal_output() {
        assert!(model_name("synthetic-sol").is_ok());
        assert!(model_name("synthetic\nsecret").is_err());
        assert!(model_name("\u{1b}[31m").is_err());
    }
}
