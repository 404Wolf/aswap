//! Claude Code: `CLAUDE_CONFIG_DIR`, `.credentials.json`, the account part of
//! `.claude.json`, transcripts under `projects/` and Anthropic's OAuth usage
//! endpoint.
use super::{Claims, Refresh, SessionArg, Spec, text};
use crate::{
    Result,
    session::SessionId,
    store,
    usage::{Failure, Usage},
};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    time::SystemTime,
};

/// Public OAuth application identifier from Claude's published client.
pub const CLIENT: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";

pub struct Claude;
impl Spec for Claude {
    fn name(&self) -> &'static str {
        "claude"
    }
    fn home_dir(&self) -> &'static str {
        ".claude"
    }
    fn env_var(&self) -> &'static str {
        "CLAUDE_CONFIG_DIR"
    }
    fn bin_var(&self) -> &'static str {
        "ASWAP_CLAUDE_BIN"
    }
    fn auth_file(&self) -> &'static str {
        ".credentials.json"
    }
    fn access_token<'a>(&self, auth: &'a Value) -> Option<&'a str> {
        auth.pointer("/claudeAiOauth/accessToken")?.as_str()
    }
    fn expires_at(&self, auth: &Value) -> Option<u64> {
        auth.pointer("/claudeAiOauth/expiresAt")?
            .as_u64()
            .map(|t| t / 1000)
    }
    fn refresh(&self, auth: &Value) -> std::result::Result<Refresh, Failure> {
        let token = auth
            .pointer("/claudeAiOauth/refreshToken")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or(Failure::new("refresh-invalid-grant"))?;
        let scopes = auth
            .pointer("/claudeAiOauth/scopes")
            .and_then(Value::as_array)
            .map(|s| s.iter().filter_map(Value::as_str).collect::<Vec<_>>())
            .filter(|s| !s.is_empty())
            .unwrap_or(vec![
                "user:profile",
                "user:inference",
                "user:sessions:claude_code",
                "user:mcp_servers",
                "user:file_upload",
            ]);
        Ok(Refresh::json(
            TOKEN_URL,
            json!({"grant_type":"refresh_token","refresh_token":token,"client_id":CLIENT,"scope":scopes.join(" ")}),
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
        let oauth = auth
            .get_mut("claudeAiOauth")
            .and_then(Value::as_object_mut)
            .ok_or(Failure::new("refresh-invalid-response"))?;
        let seconds = response
            .get("expires_in")
            .and_then(Value::as_u64)
            .filter(|s| *s > 0)
            .ok_or(Failure::new("refresh-invalid-response"))?;
        oauth.insert("accessToken".into(), json!(access));
        oauth.insert(
            "expiresAt".into(),
            json!(at.saturating_add(seconds).saturating_mul(1000)),
        );
        if let Some(refresh) = response
            .get("refresh_token")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            oauth.insert("refreshToken".into(), json!(refresh));
        }
        if let Some(scope) = response.get("scope").and_then(Value::as_str) {
            oauth.insert(
                "scopes".into(),
                json!(scope.split_whitespace().collect::<Vec<_>>()),
            );
        }
        Ok(auth)
    }
    fn strip_refresh(&self, auth: &mut Value) {
        if let Some(o) = auth.get_mut("claudeAiOauth").and_then(Value::as_object_mut) {
            o.remove("refreshToken");
        }
    }
    /// Native Claude's 401 recovery first requires a nonempty refresh field,
    /// then reloads a changed access token from disk. A public non-credential
    /// sentinel enables that reload path without distributing a
    /// refresh-capable token. Suppress proactive refresh of the sentinel;
    /// actual JWT validity is unchanged.
    fn launch_auth(&self, mut auth: Value) -> Value {
        if let Some(o) = auth.get_mut("claudeAiOauth").and_then(Value::as_object_mut) {
            o.insert("refreshToken".into(), json!("aswap-owner-managed"));
            if let Some(expiry) = o.get("expiresAt").and_then(Value::as_u64) {
                o.insert("expiresAt".into(), json!(expiry.saturating_add(86400000)));
            }
        }
        auth
    }
    fn login_fields(&self) -> &'static [&'static str] {
        &["claudeAiOauth"]
    }
    fn claims(&self, auth: &Value, config: &Value) -> Claims {
        Claims {
            email: text(config, &["/oauthAccount/emailAddress"]),
            plan: text(
                auth,
                &[
                    "/claudeAiOauth/subscriptionType",
                    "/claudeAiOauth/rateLimitTier",
                ],
            )
            .or_else(|| text(config, &["/oauthAccount/billingType"])),
            account: text(
                config,
                &["/oauthAccount/accountUuid", "/oauthAccount/emailAddress"],
            ),
            organization: text(config, &["/oauthAccount/organizationUuid"]),
        }
    }
    fn account_config(&self) -> Option<&'static str> {
        Some(".claude.json")
    }
    fn capture(&self, source: &Path, profile: &Path) -> Result<()> {
        let path = super::global_config(source, true)?;
        if path.exists() {
            let source: Value = store::json_read(&path)?;
            let mut config = json!({"hasCompletedOnboarding":true});
            config.as_object_mut().unwrap().extend(
                crate::shared::account_config(&source)
                    .as_object()
                    .unwrap()
                    .clone(),
            );
            store::json_write(&profile.join(".claude.json"), &config)?;
        }
        Ok(())
    }
    fn same_login(
        &self,
        _live: &Value,
        live_config: Option<&Value>,
        _saved: &Value,
        profile: &Path,
    ) -> Result<bool> {
        let path = profile.join(".claude.json");
        let saved_config: Value = if path.exists() {
            store::json_read(&path)?
        } else {
            json!({})
        };
        let identity = live_config.and_then(|c| c.get("oauthAccount")).filter(|v| {
            ["accountUuid", "emailAddress"].iter().any(|key| {
                v.get(key)
                    .and_then(Value::as_str)
                    .is_some_and(|s| !s.is_empty())
            })
        });
        Ok(identity.is_some() && identity == saved_config.get("oauthAccount"))
    }
    fn activates_live(&self) -> bool {
        true
    }
    fn mcp_oauth(&self) -> bool {
        true
    }
    fn shared_dirs(&self) -> &'static [&'static str] {
        &[
            "projects",
            "sessions",
            "todos",
            "shell-snapshots",
            "statsig",
            "plans",
            "file-history",
            "skills",
            "plugins",
            "agents",
            "commands",
            "tasks",
            "debug",
            "cache",
        ]
    }
    fn history_file(&self) -> Option<&'static str> {
        Some("history.jsonl")
    }
    fn transcripts(&self) -> &'static str {
        "projects"
    }
    fn transcript_depth(&self) -> usize {
        1
    }
    fn transcript_id<'a>(&self, stem: &'a str) -> Option<&'a str> {
        Some(stem)
    }
    fn session_arg(&self, key: &str) -> Option<SessionArg> {
        match key {
            "--resume" | "-r" | "--session-id" => Some(SessionArg::Id),
            "--continue" | "-c" => Some(SessionArg::Continue),
            "--fork-session" => Some(SessionArg::Fork),
            "--print" | "-p" => Some(SessionArg::OneShot),
            _ => None,
        }
    }
    fn new_session_flag(&self) -> Option<&'static str> {
        Some("--session-id")
    }
    fn resume_args(&self, id: &SessionId) -> Vec<String> {
        vec!["--resume".into(), id.to_string()]
    }
    /// Claude's published CLI registers `sessions/<pid>.json` with its
    /// session ID: it names the main conversation even while subagents hold
    /// other transcripts open.
    fn session_registry(&self, home: &Path, pid: u32) -> Option<PathBuf> {
        Some(home.join("sessions").join(format!("{pid}.json")))
    }
    fn registered_session(&self, registry: &Value, pid: u32) -> Option<SessionId> {
        (registry.get("pid")?.as_u64()? == u64::from(pid))
            .then(|| {
                registry
                    .get("sessionId")?
                    .as_str()
                    .and_then(SessionId::parse)
            })
            .flatten()
    }
    fn trusts_known_session(&self) -> bool {
        true
    }
    fn records_limits(&self) -> bool {
        true
    }
    /// Claude appends `isApiErrorMessage: true, error: "rate_limit"` the
    /// moment a request hits any limit, and never writes it again when it
    /// redraws the conversation. A compaction that failed on a limit is a
    /// `prompt_too_long` assistant error whose text names the limit.
    fn limit_entry(&self, entry: &Value, since: SystemTime) -> bool {
        entry.get("isApiErrorMessage").and_then(Value::as_bool) == Some(true)
            && super::stamped_since(entry.get("timestamp"), since)
            && match entry.get("error").and_then(Value::as_str) {
                Some("rate_limit") => true,
                Some("prompt_too_long") if entry["type"] == "assistant" => entry
                    .pointer("/message/content")
                    .and_then(Value::as_array)
                    .is_some_and(|blocks| {
                        blocks.iter().any(|block| {
                            block["type"] == "text"
                                && block["text"].as_str().is_some_and(|text| {
                                    crate::terminal::claude_compaction_limit(
                                        &text.to_lowercase().replace(['’', '‘'], "'"),
                                    )
                                })
                        })
                    }),
                _ => false,
            }
    }
    fn usage(&self, profile: &Path) -> std::result::Result<Usage, Failure> {
        crate::usage::fetch_claude(profile)
    }
}
