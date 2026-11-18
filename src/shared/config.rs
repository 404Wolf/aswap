//! Claude global config: which keys belong to the account, and the leaf-wise
//! three-way merge that publishes a view's edits.
use super::exists;
use crate::{Error, Result, Safe};
use serde_json::{Value, json};
use std::{collections::BTreeSet, fs::File, io::Read, path::Path};

// Based on the published Claude CLI's login/logout, identity and quota caches.
// Unknown account/auth-prefixed fields are private too; never copy default auth.
pub fn account_key(key: &str) -> bool {
    matches!(
        key,
        "oauthAccount"
            | "userID"
            | "anonymousId"
            | "primaryApiKey"
            | "customApiKeyResponses"
            | "hasAvailableSubscription"
            | "subscriptionNoticeCount"
            | "hasCompletedOnboarding"
            | "cachedExtraUsageDisabledReason"
            | "cachedStatsigGates"
            | "cachedDynamicConfigs"
            | "cachedGrowthBookFeatures"
    ) || [
        "oauth",
        "account",
        "organization",
        "subscription",
        "auth",
        "cachedOAuth",
        "cachedAccount",
        "cachedSubscription",
    ]
    .iter()
    .any(|prefix| key.starts_with(prefix))
}
pub fn account_config(config: &Value) -> Value {
    Value::Object(
        config
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(k, _)| account_key(k))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    )
}
// Claude's fullscreen boot canary tracks process liveness. Copying it between
// launches makes stale pids look like failed boots after an account switch.
pub(super) const PER_PROCESS_KEYS: &[&str] = &[
    "fullscreenBootPending",
    "fullscreenBootStrikes",
    "fullscreenAutoDisabled",
];
pub(super) fn shared_config(config: &Value) -> Value {
    Value::Object(
        config
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(k, _)| !account_key(k) && !PER_PROCESS_KEYS.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    )
}
pub(super) fn read_config(path: &Path) -> Result<Value> {
    if !exists(path) {
        return Ok(json!({}));
    }
    let mut bytes = Vec::new();
    File::open(path)
        .safe("cannot read shared config")?
        .take(4 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .safe("cannot read shared config")?;
    if bytes.len() > 4 * 1024 * 1024 {
        return Err(Error("shared config exceeds size limit"));
    }
    let value: Value =
        serde_json::from_slice(&bytes).safe("invalid shared config; contents redacted")?;
    if !value.is_object() {
        return Err(Error("provider config must be an object"));
    }
    Ok(value)
}
/// Leaf-wise optimistic merge: preserve concurrent edits; unchanged keys do not
/// get written back. Account fields never enter this merge.
pub fn merge_changes(base: &Value, after: &Value, current: &mut Value) {
    if base == after {
        return;
    }
    if let (Some(base), Some(after), Some(current)) =
        (base.as_object(), after.as_object(), current.as_object_mut())
    {
        let keys: BTreeSet<_> = base.keys().chain(after.keys()).collect();
        for key in keys {
            match (base.get(key), after.get(key), current.get_mut(key)) {
                (Some(b), Some(a), Some(c)) => merge_changes(b, a, c),
                (None, Some(a), None) => {
                    current.insert(key.clone(), a.clone());
                }
                (Some(b), None, Some(c)) if b == c => {
                    current.remove(key);
                }
                _ => {}
            }
        }
    } else if current == base {
        *current = after.clone();
    }
}
pub(super) fn merge_missing(source: &Value, dest: &mut Value) {
    if let (Some(source), Some(dest)) = (source.as_object(), dest.as_object_mut()) {
        for (k, v) in source {
            if let Some(old) = dest.get_mut(k) {
                merge_missing(v, old);
            } else {
                dest.insert(k.clone(), v.clone());
            }
        }
    }
}

#[cfg(test)]
mod config_tests {
    use super::*;
    #[test]
    fn snapshot_omits_only_account_and_top_level_process_keys() {
        let config = json!({
            "oauthAccount": {"accountUuid": "synthetic"},
            "fullscreenBootPending": {"123": {"startedAt": 1}},
            "fullscreenBootStrikes": {"count": 2},
            "fullscreenAutoDisabled": {"strikes": 2},
            "fullscreenUpsellSeenCount": 3,
            "theme": "dark",
            "projects": {"synthetic": {"fullscreenBootPending": true}}
        });
        assert_eq!(
            shared_config(&config),
            json!({
                "fullscreenUpsellSeenCount": 3,
                "theme": "dark",
                "projects": {"synthetic": {"fullscreenBootPending": true}}
            })
        );
    }
    #[test]
    fn process_changes_preserve_home_state_while_other_keys_merge() {
        let base = shared_config(&json!({"theme": "dark"}));
        let after = shared_config(&json!({
            "theme": "light",
            "fullscreenBootPending": {"456": {"startedAt": 2}},
            "fullscreenBootStrikes": {"count": 3},
            "fullscreenAutoDisabled": {"strikes": 3}
        }));
        for mut current in [
            json!({"theme": "dark"}),
            json!({
                "theme": "dark",
                "fullscreenBootPending": {"123": {"startedAt": 1}},
                "fullscreenBootStrikes": {"count": 1},
                "fullscreenAutoDisabled": {"strikes": 2}
            }),
        ] {
            let mut expected = current.clone();
            expected["theme"] = json!("light");
            merge_changes(&base, &after, &mut current);
            assert_eq!(current, expected);
        }
    }
}
