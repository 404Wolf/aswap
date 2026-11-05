//! MCP server OAuth belongs to the user, not to an account: it survives account
//! switches, native writes in a launch view merge back instead of being
//! overwritten, and MCP refresh tokens stay in the view.
use aswap::{
    provider::{self, Provider},
    shared::View,
    store::{self, Store},
};
use serde_json::{Value, json};
use std::process::Command;
#[test]
fn mcp_oauth_is_shared_across_views_and_accounts() {
    let t = tempfile::tempdir().unwrap();
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "mcp_child", "--nocapture"])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("HOME", t.path())
        .env("XDG_DATA_HOME", t.path().join("data"))
        .env("CLAUDE_CONFIG_DIR", t.path().join("claude"))
        .env("CODEX_HOME", t.path().join("codex"))
        .env("ASWAP_MCP_CHILD", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
fn auth(view: &View) -> Value {
    store::json_read(&view.path.join(".credentials.json")).unwrap()
}
fn edit(view: &View, change: impl FnOnce(&mut Value)) {
    let mut value = auth(view);
    change(&mut value);
    store::json_write(&view.path.join(".credentials.json"), &value).unwrap();
}
#[test]
fn mcp_child() {
    if std::env::var("ASWAP_MCP_CHILD").as_deref() != Ok("1") {
        return;
    }
    let store = Store::from_env().unwrap();
    let home = Provider::Claude.current_home().unwrap();
    let mut accounts = Vec::new();
    for (name, token) in [("one", "synthetic-first"), ("two", "synthetic-second")] {
        store::json_write(
            &home.join(".credentials.json"),
            &json!({"claudeAiOauth":{"accessToken":token,"refreshToken":"synthetic-account-refresh"}}),
        )
        .unwrap();
        accounts.push(provider::add(&store, Provider::Claude, name).unwrap());
    }
    // A copy left in a profile by an earlier release seeds the shared record.
    let profile = store
        .profile(Provider::Claude, "one")
        .unwrap()
        .join(".credentials.json");
    let mut legacy: Value = store::json_read(&profile).unwrap();
    legacy["mcpOAuth"] =
        json!({"legacy|1":{"serverName":"legacy","accessToken":"synthetic-legacy","expiresAt":5}});
    store::json_write(&profile, &legacy).unwrap();

    let mut a = View::prepare(&store, &accounts[0]).unwrap();
    let mut b = View::prepare(&store, &accounts[1]).unwrap();
    assert_eq!(
        auth(&b)["mcpOAuth"]["legacy|1"]["accessToken"],
        "synthetic-legacy"
    );

    // A login in view a: the client registration lands before the browser
    // callback and must survive the periodic sync, then the tokens follow.
    edit(&a, |v| {
        v["mcpOAuth"]["linear|1"] = json!({"serverName":"linear","clientId":"synthetic-client"})
    });
    a.sync(&store, &accounts[0]).unwrap();
    assert_eq!(
        auth(&a)["mcpOAuth"]["linear|1"]["clientId"],
        "synthetic-client"
    );
    edit(&a, |v| {
        let entry = &mut v["mcpOAuth"]["linear|1"];
        entry["accessToken"] = json!("synthetic-access");
        entry["refreshToken"] = json!("synthetic-mcp-refresh");
    });
    a.sync(&store, &accounts[0]).unwrap();
    let after = auth(&a);
    assert_eq!(
        after["mcpOAuth"]["linear|1"]["refreshToken"],
        "synthetic-mcp-refresh"
    );
    assert_eq!(
        after["claudeAiOauth"]["refreshToken"],
        "aswap-owner-managed"
    );

    // A session on the other account receives it, and its rotation flows back.
    b.sync(&store, &accounts[1]).unwrap();
    assert_eq!(
        auth(&b)["mcpOAuth"]["linear|1"]["accessToken"],
        "synthetic-access"
    );
    edit(&b, |v| {
        v["mcpOAuth"]["linear|1"]["accessToken"] = json!("synthetic-rotated")
    });
    b.sync(&store, &accounts[1]).unwrap();
    a.sync(&store, &accounts[0]).unwrap();
    assert_eq!(
        auth(&a)["mcpOAuth"]["linear|1"]["accessToken"],
        "synthetic-rotated"
    );
    assert_eq!(
        auth(&a)["mcpOAuth"]["linear|1"]["clientId"],
        "synthetic-client"
    );

    // An account switch keeps MCP auth, and a removal in one view propagates.
    a.select(&store, &accounts[1]).unwrap();
    assert_eq!(auth(&a)["claudeAiOauth"]["accessToken"], "synthetic-second");
    assert_eq!(
        auth(&a)["mcpOAuth"]["linear|1"]["accessToken"],
        "synthetic-rotated"
    );
    edit(&b, |v| {
        v["mcpOAuth"].as_object_mut().unwrap().remove("legacy|1");
    });
    b.sync(&store, &accounts[1]).unwrap();
    a.sync(&store, &accounts[1]).unwrap();
    assert!(auth(&a)["mcpOAuth"].get("legacy|1").is_none());
    assert_eq!(
        auth(&a)["mcpOAuth"]["linear|1"]["accessToken"],
        "synthetic-rotated"
    );
}
