//! OpenAI Codex CLI: `CODEX_HOME`, `auth.json`, rollouts under `sessions/`
//! and quota from its app-server.
use super::{Claims, Refresh, SessionArg, Spec, Subcommand, jwt_claims, text};
use crate::{
    Result, store,
    usage::{Failure, Usage},
};
use serde_json::{Value, json};
use std::path::Path;

/// Public OAuth application identifier from Codex's published client.
pub const CLIENT: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub(super) const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";

pub struct Codex;
impl Spec for Codex {
    fn name(&self) -> &'static str {
        "codex"
    }
    fn home_dir(&self) -> &'static str {
        ".codex"
    }
    fn env_var(&self) -> &'static str {
        "CODEX_HOME"
    }
    fn bin_var(&self) -> &'static str {
        "ASWAP_CODEX_BIN"
    }
    fn launch_args(&self) -> &'static [&'static str] {
        &["-c", "cli_auth_credentials_store=\"file\""]
    }
    fn auth_file(&self) -> &'static str {
        "auth.json"
    }
    fn auth_valid(&self, auth: &Value) -> bool {
        let string = |v: Option<&Value>| v.and_then(Value::as_str).is_some_and(|s| !s.is_empty());
        string(auth.pointer("/tokens/access_token")) || string(auth.get("OPENAI_API_KEY"))
    }
    fn access_token<'a>(&self, auth: &'a Value) -> Option<&'a str> {
        auth.pointer("/tokens/access_token")?.as_str()
    }
    fn expires_at(&self, auth: &Value) -> Option<u64> {
        jwt_claims(self.access_token(auth)?)?.get("exp")?.as_u64()
    }
    /// An API-key login has nothing to refresh.
    fn refreshable(&self, auth: &Value) -> bool {
        !(auth.get("tokens").is_none_or(Value::is_null)
            && auth.get("OPENAI_API_KEY").and_then(Value::as_str).is_some())
    }
    fn refresh(&self, auth: &Value) -> std::result::Result<Refresh, Failure> {
        let token = auth
            .pointer("/tokens/refresh_token")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or(Failure::new("refresh-invalid-grant"))?;
        Ok(Refresh::json(
            TOKEN_URL,
            json!({"grant_type":"refresh_token","refresh_token":token,"client_id":CLIENT}),
        ))
    }
    fn apply_refresh(
        &self,
        old: &Value,
        response: &Value,
        at: u64,
    ) -> std::result::Result<Value, Failure> {
        let access = super::response_access(response)?;
        let mut auth = old.clone();
        let tokens = auth
            .get_mut("tokens")
            .and_then(Value::as_object_mut)
            .ok_or(Failure::new("refresh-invalid-response"))?;
        tokens.insert("access_token".into(), json!(access));
        for key in ["refresh_token", "id_token"] {
            if let Some(value) = response
                .get(key)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                tokens.insert(key.into(), json!(value));
            }
        }
        auth["last_refresh"] = json!(
            chrono::DateTime::from_timestamp(at as i64, 0)
                .ok_or(Failure::new("refresh-invalid-response"))?
                .to_rfc3339()
        );
        Ok(auth)
    }
    fn strip_refresh(&self, auth: &mut Value) {
        if let Some(o) = auth.get_mut("tokens").and_then(Value::as_object_mut) {
            o.insert("refresh_token".into(), json!(""));
        }
    }
    /// Launch views use Codex's external `chatgptAuthTokens` mode, so Codex
    /// never tries to refresh the (empty) refresh token itself.
    fn launch_auth(&self, mut auth: Value) -> Value {
        if self
            .access_token(&auth)
            .is_some_and(|token| !token.is_empty())
            && !auth
                .get("OPENAI_API_KEY")
                .and_then(Value::as_str)
                .is_some_and(|key| !key.is_empty())
        {
            auth["auth_mode"] = json!("chatgptAuthTokens");
        }
        auth
    }
    fn login_fields(&self) -> &'static [&'static str] {
        &["tokens", "last_refresh"]
    }
    fn claims(&self, auth: &Value, _config: &Value) -> Claims {
        let id = auth
            .pointer("/tokens/id_token")
            .and_then(Value::as_str)
            .and_then(jwt_claims)
            .unwrap_or(Value::Null);
        let access = self
            .access_token(auth)
            .and_then(jwt_claims)
            .unwrap_or(Value::Null);
        Claims {
            email: text(&id, &["/email"]).or_else(|| text(auth, &["/email"])),
            plan: text(&id, &[PLAN]).or_else(|| text(&access, &[PLAN])),
            account: text(auth, &["/tokens/account_id"]).or_else(|| {
                text(
                    &id,
                    &[
                        "/https:~1~1api.openai.com~1auth/chatgpt_account_id",
                        "/sub",
                        "/email",
                    ],
                )
            }),
            organization: None,
        }
    }
    fn capture(&self, _source: &Path, profile: &Path) -> Result<()> {
        store::atomic(
            &profile.join("config.toml"),
            b"cli_auth_credentials_store = \"file\"\n",
        )
    }
    fn same_login(
        &self,
        live: &Value,
        _live_config: Option<&Value>,
        saved: &Value,
        _profile: &Path,
    ) -> Result<bool> {
        let identity = live
            .pointer("/tokens/account_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        Ok(identity.is_some()
            && identity == saved.pointer("/tokens/account_id").and_then(Value::as_str))
    }
    fn activates_live(&self) -> bool {
        true
    }
    fn shared_dirs(&self) -> &'static [&'static str] {
        &[
            "sessions",
            "archived_sessions",
            "skills",
            "plugins",
            "rules",
            "memories",
            "tmp",
            "logs",
        ]
    }
    fn history_file(&self) -> Option<&'static str> {
        Some("history.jsonl")
    }
    fn transcripts(&self) -> &'static str {
        "sessions"
    }
    fn transcript_depth(&self) -> usize {
        5
    }
    /// `rollout-<time>-<UUID>.jsonl`.
    fn transcript_id<'a>(&self, stem: &'a str) -> Option<&'a str> {
        stem.get(stem.len().checked_sub(36)?..)
    }
    fn subcommand(&self, first: &str) -> Option<Subcommand> {
        match first {
            "resume" => Some(Subcommand::default()),
            "fork" => Some(Subcommand {
                fork: true,
                ..Default::default()
            }),
            "exec" => Some(Subcommand {
                one_shot: true,
                ..Default::default()
            }),
            _ => None,
        }
    }
    fn session_arg(&self, key: &str) -> Option<SessionArg> {
        (key == "--last").then_some(SessionArg::Continue)
    }
    fn takes_value(&self, key: &str) -> bool {
        matches!(key, "-c" | "-p")
    }
    fn resume_args(&self, id: &str) -> Vec<String> {
        vec!["resume".into(), id.into()]
    }
    fn model_catalog(&self) -> bool {
        true
    }
    fn usage(&self, profile: &Path) -> std::result::Result<Usage, Failure> {
        // The usage-only app-server cannot rotate auth behind a running CLI.
        crate::usage::fetch_codex(
            store::json_read::<Value>(&profile.join(self.auth_file()))
                .map(|auth| crate::refresh::access_only(super::Provider::Codex, &auth)),
        )
    }
}
const PLAN: &str = "/https:~1~1api.openai.com~1auth/chatgpt_plan_type";
