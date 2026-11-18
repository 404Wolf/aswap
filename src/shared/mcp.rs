//! The store-wide Claude MCP OAuth record shared by every launch view.
use crate::{
    Result,
    provider::Provider,
    store::{self, Store},
};
use serde_json::Value;

/// Claude keeps MCP server OAuth (client registrations, access and refresh
/// tokens) beside the account login in .credentials.json. It belongs to the
/// user, not to the account: one store-wide record feeds every launch view and
/// survives account switches, and native logins, registrations and rotations
/// in a view merge back into it. aswap never refreshes MCP tokens, so views
/// keep their MCP refresh tokens exactly as a native Claude home would.
pub(super) const MCP_OAUTH: &str = "mcpOAuth";
pub(super) fn mcp_record(store: &Store) -> Result<Value> {
    let path = store.mcp_auth(Provider::Claude);
    if path.exists() {
        return store::json_read(&path);
    }
    // First use: adopt what earlier releases left in the profiles. Those
    // copies lost their refresh tokens; the latest expiry per server wins.
    let expiry = |entry: &Value| entry.get("expiresAt").and_then(Value::as_u64);
    let mut seed = serde_json::Map::new();
    for account in store.load()?.accounts {
        if account.provider != Provider::Claude {
            continue;
        }
        let profile = store.profile(account.provider, &account.label)?;
        let Ok(auth) = store::json_read::<Value>(&profile.join(account.provider.auth_file()))
        else {
            continue;
        };
        if let Some(servers) = auth.get(MCP_OAUTH).and_then(Value::as_object) {
            for (server, entry) in servers {
                if seed
                    .get(server)
                    .is_none_or(|old| expiry(entry) > expiry(old))
                {
                    seed.insert(server.clone(), entry.clone());
                }
            }
        }
    }
    let seed = Value::Object(seed);
    store::json_write(&path, &seed)?;
    Ok(seed)
}
pub(super) fn with_mcp(mut auth: Value, record: &Value) -> Value {
    if let Some(auth) = auth.as_object_mut() {
        if record.as_object().is_some_and(|r| !r.is_empty()) {
            auth.insert(MCP_OAUTH.into(), record.clone());
        } else {
            auth.remove(MCP_OAUTH);
        }
    }
    auth
}
