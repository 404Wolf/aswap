//! pi's ChatGPT login, parsed once from `auth.json` into types.
//!
//! pi's `auth.json` maps a pi provider ID to a credential. A ChatGPT
//! subscription login is an OAuth credential under one of two IDs (see
//! [`Kind`]); everything else in the file (API keys, other providers' OAuth)
//! is the user's and is never parsed or kept here.
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// What can be wrong with a pi login.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("pi's auth.json has no ChatGPT login; run /login in pi first")]
    Missing,
    #[error("pi's ChatGPT login is malformed")]
    Malformed,
    #[error("pi's Sign in with ChatGPT login has no client ID; log in again in pi")]
    NoClient,
    #[error("pi's ChatGPT login has no refresh token")]
    NoRefresh,
}

/// Which pi provider holds the login.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `openai-codex`: the legacy login through Codex's OAuth client. Its
    /// token works with the Codex backend, so quota can be read.
    Codex,
    /// `openai`: Sign in with ChatGPT, with a client registered per login and
    /// a token for api.openai.com.
    ChatGpt,
}
impl Kind {
    /// In the order `aswap add pi` looks for a login.
    pub const ALL: [Kind; 2] = [Kind::Codex, Kind::ChatGpt];
    /// The key in pi's `auth.json`.
    pub fn id(self) -> &'static str {
        match self {
            Kind::Codex => "openai-codex",
            Kind::ChatGpt => "openai",
        }
    }
}

/// An OAuth access token. Never printed: no `Debug`, no `Display`.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AccessToken(String);
impl AccessToken {
    pub fn expose(&self) -> &str {
        &self.0
    }
}
/// An OAuth refresh token, or the empty string an access-only copy carries.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RefreshToken(String);
impl RefreshToken {
    /// The empty token of an access-only copy.
    pub fn none() -> Self {
        Self(String::new())
    }
    /// The public, non-credential token of a launch view: pi requires one,
    /// and it can never authorize a rotation.
    pub fn sentinel() -> Self {
        Self("aswap-owner-managed".into())
    }
    pub fn new(token: String) -> Self {
        Self(token)
    }
    /// The real token, if this is one.
    pub fn expose(&self) -> Option<&str> {
        (!self.0.is_empty() && *self != Self::sentinel()).then_some(&self.0)
    }
}
/// A Unix time in milliseconds, as pi stores `expires`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EpochMillis(u64);
impl EpochMillis {
    pub fn from_secs(secs: u64) -> Self {
        Self(secs.saturating_mul(1000))
    }
    pub fn secs(self) -> u64 {
        self.0 / 1000
    }
    pub fn later(self, millis: u64) -> Self {
        Self(self.0.saturating_add(millis))
    }
}
/// The ChatGPT account (workspace) a token belongs to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AccountId(String);
impl AccountId {
    pub fn new(id: String) -> Option<Self> {
        (!id.is_empty()).then_some(Self(id))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
/// The OAuth client a Sign in with ChatGPT login registered.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ClientId(String);
impl ClientId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
enum Type {
    #[serde(rename = "oauth")]
    OAuth,
}
/// One OAuth credential as pi stores it. Fields aswap doesn't know survive
/// a round trip.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuth {
    #[serde(rename = "type")]
    kind: Type,
    pub access: AccessToken,
    pub refresh: RefreshToken,
    pub expires: EpochMillis,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<AccountId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<ClientId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scopes: Option<Vec<String>>,
    #[serde(flatten)]
    rest: Map<String, Value>,
}

/// A parsed ChatGPT login: which pi provider holds it, and the credential.
#[derive(Clone)]
pub struct Login {
    pub kind: Kind,
    pub oauth: OAuth,
}
impl Login {
    /// The first ChatGPT login in a pi `auth.json` (or an aswap profile,
    /// which holds only that entry).
    pub fn parse(auth: &Value) -> Result<Self, Error> {
        let mut present = false;
        for kind in Kind::ALL {
            let Some(entry) = auth.get(kind.id()) else {
                continue;
            };
            present |= entry.get("type").and_then(Value::as_str) == Some("oauth");
            if let Ok(oauth) = serde_json::from_value::<OAuth>(entry.clone())
                && !oauth.access.0.is_empty()
            {
                return Ok(Self { kind, oauth });
            }
        }
        Err(if present {
            Error::Malformed
        } else {
            Error::Missing
        })
    }
    /// The login as a one-entry `auth.json`.
    pub fn to_value(&self) -> Value {
        let mut map = Map::new();
        map.insert(
            self.kind.id().into(),
            serde_json::to_value(&self.oauth).expect("a pi login encodes"),
        );
        Value::Object(map)
    }
    /// The real refresh token.
    pub fn refresh_token(&self) -> Result<&str, Error> {
        self.oauth.refresh.expose().ok_or(Error::NoRefresh)
    }
    /// The registered client of a Sign in with ChatGPT login.
    pub fn client(&self) -> Result<&ClientId, Error> {
        self.oauth.client_id.as_ref().ok_or(Error::NoClient)
    }
}
