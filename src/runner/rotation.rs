//! Whether a login error raced a token rotation rather than an expired account.
use crate::{
    provider::Provider,
    store::{self, Account, Store},
};
use std::time::{Duration, Instant};

/// A digest of an auth file's access token: enough to tell that the token
/// changed, without keeping the token itself in memory.
pub(super) fn access_print(provider: Provider, path: &std::path::Path) -> Option<Vec<u8>> {
    let auth: serde_json::Value = store::json_read(path).ok()?;
    let pointer = if provider == Provider::Claude {
        "/claudeAiOauth/accessToken"
    } else {
        "/tokens/access_token"
    };
    let token = auth.pointer(pointer)?.as_str()?;
    Some(
        ring::digest::digest(&ring::digest::SHA256, token.as_bytes())
            .as_ref()
            .to_vec(),
    )
}
/// Whether the account's token differs from the one the provider started with.
/// Only then is a login error a rotation race rather than an expired account;
/// once a provider started with the current token also fails, it is real.
pub(super) fn rotated_since(store: &Store, account: &Account, launched: Option<&[u8]>) -> bool {
    let Some(launched) = launched else {
        return false;
    };
    store
        .profile(account.provider, &account.label)
        .is_ok_and(|profile| {
            access_print(
                account.provider,
                &profile.join(account.provider.auth_file()),
            )
            .is_some_and(|now| now != launched)
        })
}
pub(super) fn rotation_by(
    store: &Store,
    account: &Account,
    launched: Option<&[u8]>,
    deadline: Instant,
) -> bool {
    loop {
        if rotated_since(store, account, launched) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}
