//! pi (pi.dev) with its own ChatGPT login: `PI_CODING_AGENT_DIR` (default
//! `~/.pi/agent`), `auth.json`, sessions under `sessions/--<cwd>--/`.
//!
//! An aswap pi account is one ChatGPT subscription login ([`login::Login`]);
//! a profile's `auth.json` holds only that entry.
//!
//! pi refreshes a token once `expires` is less than five minutes away, under
//! a lock on its own `auth.json`, and re-reads the file whenever it changes.
//! A launch view's entry therefore carries a far-future `expires` and a
//! sentinel refresh token: pi never refreshes, aswap refreshes the profile
//! and hands the new token to every view, and pi picks it up on its next
//! request.
pub mod login;

use super::{Claims, Encoding, Refresh, SessionArg, Spec, codex, jwt_claims, text};
use crate::{
    Result,
    session::SessionId,
    store,
    usage::{Failure, Usage},
};
use login::{AccountId, EpochMillis, Kind, Login, RefreshToken};
use serde_json::{Value, json};
use std::{
    path::Path,
    time::{Duration, Instant, SystemTime},
};

/// Sign in with ChatGPT's token endpoint and resource (pi-ai's
/// `openai-chatgpt.js`); its refresh is form-encoded.
const CHATGPT_TOKEN_URL: &str = "https://auth.openai.com/api/accounts/oauth/token";
const CHATGPT_RESOURCE: &str = "https://api.openai.com/v1";
const ACCOUNT: &str = "/https:~1~1api.openai.com~1auth/chatgpt_account_id";
const PLAN: &str = "/https:~1~1api.openai.com~1auth/chatgpt_plan_type";
const EMAIL: &str = "/https:~1~1api.openai.com~1profile/email";
/// How long a launch view's login outlives the real expiry inside pi.
const VIEW_GRACE_MS: u64 = 86_400_000;
/// How long `aswap add pi` waits for a running pi to release its lock.
const LOCK_PATIENCE: Duration = Duration::from_secs(5);

/// The account a Codex-backend access token belongs to.
fn token_account(access: &str) -> Option<AccountId> {
    AccountId::new(text(&jwt_claims(access)?, &[ACCOUNT])?)
}
/// Apply `change` to the login in `auth`; anything that is not a login is
/// returned unchanged.
fn with_login(auth: Value, change: impl FnOnce(&mut Login)) -> Value {
    match Login::parse(&auth) {
        Ok(mut login) => {
            change(&mut login);
            login.to_value()
        }
        Err(_) => auth,
    }
}

/// pi's `Spec`.
pub struct Pi;
impl Spec for Pi {
    fn name(&self) -> &'static str {
        "pi"
    }
    fn home_dir(&self) -> &'static str {
        ".pi/agent"
    }
    fn env_var(&self) -> &'static str {
        "PI_CODING_AGENT_DIR"
    }
    fn bin_var(&self) -> &'static str {
        crate::runner::PI_BIN
    }
    fn auth_file(&self) -> &'static str {
        "auth.json"
    }
    /// Only the ChatGPT login: other entries (API keys, other providers'
    /// OAuth) stay the user's and never enter a profile.
    fn select_login(&self, auth: Value) -> Value {
        Login::parse(&auth).map_or(json!({}), |login| login.to_value())
    }
    /// Move, not copy: pi would go on refreshing the same login in its own
    /// `auth.json`, and the first rotation would break the other copy. The
    /// entry is removed under pi's own lock (proper-lockfile's
    /// `auth.json.lock` directory); every other entry stays.
    fn release(&self, source: &Path, saved: &Value) -> Result<()> {
        let Ok(login) = Login::parse(saved) else {
            return Ok(());
        };
        let path = source.join(self.auth_file());
        store::no_links(&path)?;
        let lock = source.join("auth.json.lock");
        let deadline = Instant::now() + LOCK_PATIENCE;
        while std::fs::create_dir(&lock).is_err() {
            if Instant::now() >= deadline {
                return Err(crate::Error(
                    "pi's auth.json is locked by a running pi; retry when it is idle",
                ));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let result = (|| {
            let mut auth: Value = store::json_read(&path)?;
            let id = login.kind.id();
            if auth.get(id) == login.to_value().get(id)
                && let Some(entries) = auth.as_object_mut()
            {
                entries.remove(id);
                store::json_write(&path, &auth)?;
            }
            Ok(())
        })();
        let _ = std::fs::remove_dir(&lock);
        result
    }
    fn auth_valid(&self, auth: &Value) -> bool {
        Login::parse(auth).is_ok()
    }
    fn access_token<'a>(&self, auth: &'a Value) -> Option<&'a str> {
        let kind = Login::parse(auth).ok()?.kind;
        auth.get(kind.id())?.get("access")?.as_str()
    }
    fn expires_at(&self, auth: &Value) -> Option<u64> {
        Some(Login::parse(auth).ok()?.oauth.expires.secs())
    }
    fn refresh(&self, auth: &Value) -> std::result::Result<Refresh, Failure> {
        let invalid = |_| Failure::new("refresh-invalid-grant");
        let login = Login::parse(auth).map_err(invalid)?;
        let token = login.refresh_token().map_err(invalid)?;
        Ok(match login.kind {
            Kind::Codex => Refresh::json(
                codex::TOKEN_URL,
                json!({"grant_type":"refresh_token","refresh_token":token,"client_id":codex::CLIENT}),
            ),
            Kind::ChatGpt => Refresh {
                url: CHATGPT_TOKEN_URL,
                encoding: Encoding::Form,
                body: json!({"grant_type":"refresh_token","client_id":login.client().map_err(invalid)?.as_str(),"refresh_token":token,"resource":CHATGPT_RESOURCE}),
            },
        })
    }
    fn apply_refresh(
        &self,
        old: &Value,
        response: &Value,
        at: u64,
    ) -> std::result::Result<Value, Failure> {
        let invalid = || Failure::new("refresh-invalid-response");
        let access = super::response_access(response)?;
        let seconds = response
            .get("expires_in")
            .and_then(Value::as_u64)
            .filter(|s| *s > 0)
            .ok_or_else(invalid)?;
        let mut login = Login::parse(old).map_err(|_| invalid())?;
        login.oauth.access = serde_json::from_value(json!(access)).map_err(|_| invalid())?;
        login.oauth.expires = EpochMillis::from_secs(at.saturating_add(seconds));
        if let Some(refresh) = response
            .get("refresh_token")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            login.oauth.refresh = RefreshToken::new(refresh.into());
        }
        match login.kind {
            Kind::Codex => {
                if let Some(account) = token_account(access) {
                    login.oauth.account_id = Some(account);
                }
            }
            Kind::ChatGpt => {
                if let Some(scope) = response.get("scope").and_then(Value::as_str) {
                    login.oauth.scopes = Some(scope.split_whitespace().map(Into::into).collect());
                }
            }
        }
        Ok(login.to_value())
    }
    /// pi requires `refresh` to be a string.
    fn strip_refresh(&self, auth: &mut Value) {
        *auth = with_login(auth.take(), |login| {
            login.oauth.refresh = RefreshToken::none()
        });
    }
    /// A sentinel refresh token and an expiry a day late: pi never starts a
    /// refresh of its own (it could not succeed, and must not race aswap's).
    fn launch_auth(&self, auth: Value) -> Value {
        with_login(auth, |login| {
            login.oauth.refresh = RefreshToken::sentinel();
            login.oauth.expires = login.oauth.expires.later(VIEW_GRACE_MS);
        })
    }
    fn login_fields(&self) -> &'static [&'static str] {
        &["openai-codex", "openai"]
    }
    fn claims(&self, auth: &Value, _config: &Value) -> Claims {
        let Ok(login) = Login::parse(auth) else {
            return Claims::default();
        };
        let access = jwt_claims(login.oauth.access.expose()).unwrap_or(Value::Null);
        Claims {
            email: text(&access, &[EMAIL, "/email"]),
            plan: text(&access, &[PLAN]),
            account: login
                .oauth
                .account_id
                .map(|a| a.as_str().to_owned())
                .or_else(|| text(&access, &[ACCOUNT, "/sub", "/email"])),
            organization: None,
        }
    }
    fn shared_dirs(&self) -> &'static [&'static str] {
        &["sessions"]
    }
    fn transcripts(&self) -> &'static str {
        "sessions"
    }
    /// `sessions/--<cwd>--/<time>_<id>.jsonl`.
    fn transcript_depth(&self) -> usize {
        1
    }
    fn transcript_id<'a>(&self, stem: &'a str) -> Option<&'a str> {
        stem.get(stem.len().checked_sub(36)?..)
    }
    fn session_arg(&self, key: &str) -> Option<SessionArg> {
        match key {
            "--session" | "--session-id" => Some(SessionArg::Id),
            "--continue" | "-c" | "--resume" | "-r" => Some(SessionArg::Continue),
            "--fork" => Some(SessionArg::ForkFrom),
            // Nothing is saved, so there is nothing to resume.
            "--no-session" => Some(SessionArg::Fork),
            "--print" | "-p" => Some(SessionArg::OneShot),
            _ => None,
        }
    }
    fn takes_value(&self, key: &str) -> bool {
        matches!(
            key,
            "-e" | "--extension"
                | "--provider"
                | "--mode"
                | "--session-dir"
                | "--models"
                | "--thinking"
                | "--name"
                | "--api-key"
                | "--tools"
        )
    }
    fn new_session_flag(&self) -> Option<&'static str> {
        Some("--session-id")
    }
    /// `--session-id` opens the session with exactly this ID in the project.
    fn resume_args(&self, id: &SessionId) -> Vec<String> {
        vec!["--session-id".into(), id.to_string()]
    }
    /// pi appends with open/close, so no descriptor names its session; other
    /// pi sessions writing in the shared home cannot contradict a known ID.
    fn trusts_known_session(&self) -> bool {
        true
    }
    fn records_limits(&self) -> bool {
        true
    }
    /// pi records a failed request as an assistant message with
    /// `stopReason: "error"` and the provider's `errorMessage`; a ChatGPT
    /// limit reads "You have hit your ChatGPT usage limit ..." (legacy Codex
    /// API) or carries `subscription_sharing_..._usage_limit_exceeded` (Sign
    /// in with ChatGPT). pi never rewrites earlier entries on resume.
    fn limit_entry(&self, entry: &Value, since: SystemTime) -> bool {
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Message {
            role: String,
            stop_reason: Option<String>,
            error_message: Option<String>,
        }
        #[derive(serde::Deserialize)]
        struct Entry {
            #[serde(rename = "type")]
            kind: String,
            message: Option<Message>,
        }
        let Ok(Entry {
            kind,
            message: Some(message),
        }) = <Entry as serde::Deserialize>::deserialize(entry)
        else {
            return false;
        };
        kind == "message"
            && message.role == "assistant"
            && message.stop_reason.as_deref() == Some("error")
            && message.error_message.as_deref().is_some_and(limit_text)
            && super::stamped_since(entry.get("timestamp"), since)
    }
    /// The legacy `openai-codex` login is a Codex token: its quota comes
    /// from a usage-only Codex app-server given that access token. Sign in
    /// with ChatGPT tokens are for api.openai.com, which reports no quota.
    fn usage(&self, profile: &Path) -> std::result::Result<Usage, Failure> {
        let auth: Value = store::json_read(&profile.join(self.auth_file()))
            .map_err(|_| Failure::unavailable())?;
        let login = Login::parse(&auth).map_err(|_| Failure::unavailable())?;
        let codex = codex_auth(&login).ok_or(Failure::new("usage-unsupported"))?;
        crate::usage::fetch_codex(Ok(codex))
    }
}
/// Whether a pi error message is a subscription usage limit.
pub(crate) fn limit_text(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    text.contains("usage limit") || text.contains("usage_limit")
}
/// Access-only Codex auth for the `openai-codex` login, for a usage-only
/// Codex app-server. Codex reads the account from `account_id` and the plan
/// and email hints from `id_token`; the access token carries the same
/// claims. No refresh token: that app-server can never rotate the login.
pub(crate) fn codex_auth(login: &Login) -> Option<Value> {
    if login.kind != Kind::Codex {
        return None;
    }
    let access = login.oauth.access.expose();
    let account = login
        .oauth
        .account_id
        .clone()
        .or_else(|| token_account(access))?;
    Some(json!({
        "OPENAI_API_KEY": null,
        "tokens": {"id_token": access, "access_token": access, "refresh_token": "", "account_id": account},
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    fn jwt(claims: Value) -> String {
        format!(
            "e30.{}.synthetic",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
        )
    }
    fn home_auth() -> Value {
        json!({
            "anthropic": {"type": "api_key", "key": "synthetic-other"},
            "openai-codex": {"type": "oauth", "access": jwt(json!({
                "https://api.openai.com/auth": {"chatgpt_account_id": "acct-synthetic", "chatgpt_plan_type": "pro"},
                "https://api.openai.com/profile": {"email": "someone@example.com"},
            })), "refresh": "synthetic-refresh", "expires": 2_000_000_000_000u64, "accountId": "acct-synthetic"},
        })
    }
    #[test]
    fn a_profile_keeps_only_the_chatgpt_login() {
        let saved = Pi.select_login(home_auth());
        assert_eq!(saved.as_object().unwrap().len(), 1);
        // The login round-trips exactly, unknown fields included.
        assert_eq!(saved["openai-codex"], home_auth()["openai-codex"]);
        assert!(saved.get("openai-codex").is_some());
        assert!(Pi.auth_valid(&saved));
        assert!(
            !Pi.auth_valid(&Pi.select_login(json!({"anthropic": {"type": "api_key", "key": "k"}})))
        );
        assert_eq!(Pi.expires_at(&saved), Some(2_000_000_000));
        let claims = Pi.claims(&saved, &Value::Null);
        assert_eq!(claims.email.as_deref(), Some("someone@example.com"));
        assert_eq!(claims.plan.as_deref(), Some("pro"));
        assert_eq!(claims.account.as_deref(), Some("acct-synthetic"));
    }
    #[test]
    fn views_carry_no_refresh_token_and_never_expire_inside_pi() {
        let saved = Pi.select_login(home_auth());
        let view = crate::refresh::launch_auth(super::super::Provider::Pi, &saved);
        let entry = &view["openai-codex"];
        assert_eq!(entry["refresh"], "aswap-owner-managed");
        assert_eq!(entry["expires"], 2_000_000_000_000u64 + 86_400_000);
        assert_eq!(entry["access"], saved["openai-codex"]["access"]);
        let packet = crate::refresh::access_only(super::super::Provider::Pi, &saved);
        assert_eq!(packet["openai-codex"]["refresh"], "");
    }
    #[test]
    fn both_chatgpt_logins_refresh_at_their_own_endpoints() {
        let codex = Pi.refresh(&Pi.select_login(home_auth())).unwrap();
        assert_eq!(codex.url, codex::TOKEN_URL);
        assert!(matches!(codex.encoding, Encoding::Json));
        assert_eq!(codex.body["client_id"], codex::CLIENT);
        assert_eq!(codex.body["refresh_token"], "synthetic-refresh");
        let chatgpt = json!({"openai": {"type": "oauth", "access": "a", "refresh": "r", "expires": 1, "clientId": "client-synthetic", "scopes": []}});
        let request = Pi.refresh(&chatgpt).unwrap();
        assert_eq!(request.url, CHATGPT_TOKEN_URL);
        assert!(matches!(request.encoding, Encoding::Form));
        assert_eq!(request.body["client_id"], "client-synthetic");
        assert_eq!(request.body["resource"], CHATGPT_RESOURCE);
        let rotated = Pi
            .apply_refresh(&chatgpt, &json!({"access_token": "a2", "refresh_token": "r2", "expires_in": 3600, "scope": "openid offline_access"}), 1000)
            .unwrap();
        assert_eq!(rotated["openai"]["access"], "a2");
        assert_eq!(rotated["openai"]["refresh"], "r2");
        assert_eq!(rotated["openai"]["expires"], 4_600_000);
        assert_eq!(
            rotated["openai"]["scopes"],
            json!(["openid", "offline_access"])
        );
        assert_eq!(rotated["openai"]["clientId"], "client-synthetic");
        // An omitted refresh token keeps the old one.
        let kept = Pi
            .apply_refresh(
                &chatgpt,
                &json!({"access_token": "a3", "expires_in": 60}),
                0,
            )
            .unwrap();
        assert_eq!(kept["openai"]["refresh"], "r");
    }
    #[test]
    fn usage_reads_the_codex_login_through_codex_auth_only() {
        let saved = Pi.select_login(home_auth());
        let codex = codex_auth(&Login::parse(&saved).unwrap()).unwrap();
        assert_eq!(codex["tokens"]["account_id"], "acct-synthetic");
        assert_eq!(codex["tokens"]["refresh_token"], "");
        assert!(super::super::Provider::Codex.spec().auth_valid(&codex));
        let chatgpt =
            json!({"openai": {"type": "oauth", "access": "a", "refresh": "r", "expires": 1}});
        assert!(codex_auth(&Login::parse(&chatgpt).unwrap()).is_none());
    }
    #[test]
    fn a_limit_entry_is_an_errored_assistant_message_after_launch() {
        let since = SystemTime::now();
        let at = |offset: i64| {
            (chrono::DateTime::<chrono::Utc>::from(since) + chrono::Duration::seconds(offset))
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        };
        let entry = |offset, stop: &str, error: &str| {
            json!({"type": "message", "id": "x", "timestamp": at(offset), "message": {
                "role": "assistant", "content": [], "stopReason": stop, "errorMessage": error}})
        };
        let limit = "You have hit your ChatGPT usage limit (plus plan). Try again in ~42 min.";
        assert!(Pi.limit_entry(&entry(1, "error", limit), since));
        assert!(Pi.limit_entry(
            &entry(
                1,
                "error",
                "OpenAI API error: subscription_sharing_usage_limit_exceeded"
            ),
            since
        ));
        // Before launch (the conversation being resumed), not an error, or
        // another error: not a new limit.
        assert!(!Pi.limit_entry(&entry(-60, "error", limit), since));
        assert!(!Pi.limit_entry(&entry(1, "stop", limit), since));
        assert!(!Pi.limit_entry(&entry(1, "error", "OpenAI API error: 500"), since));
    }
}
