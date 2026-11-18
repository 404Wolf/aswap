//! Another program in front of a provider: pi driving Claude Code through
//! pi-claude-code-provider, which runs `claude -p` once per turn.
//!
//! With an account, pi gets that account's launch view as `CLAUDE_CONFIG_DIR`
//! and every turn uses it. Without one, pi's provider runs a private shim for
//! every `claude` call, which is `aswap run claude --pick`: each call picks
//! its account by quota, with plain stdio for `-p` stream-json pipes.
use crate::{
    Error, Result, Safe,
    provider::{self, Provider},
    store::{self, Store},
};
use std::{
    ffi::{OsStr, OsString},
    io::Write,
    os::unix::{
        ffi::OsStrExt,
        fs::PermissionsExt,
        process::{CommandExt, ExitStatusExt},
    },
    path::{Path, PathBuf},
    process::Command,
};

/// A program run in front of a provider.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Via {
    /// pi, with pi-claude-code-provider (Claude only).
    Pi,
}
/// The variable naming the pi binary, as `ASWAP_<PROVIDER>_BIN` does.
pub const PI_BIN: &str = "ASWAP_PI_BIN";
/// Where pi-claude-code-provider looks for its `claude` binary.
pub const PROVIDER_PATH: &str = "PI_CLAUDE_CODE_PROVIDER_PATH";

impl Via {
    pub fn check(self, provider: Provider) -> Result<()> {
        match (self, provider) {
            (Self::Pi, Provider::Claude) => Ok(()),
            _ => Err(Error("--via pi drives Claude accounts only")),
        }
    }
    fn binary(self) -> OsString {
        std::env::var_os(PI_BIN)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "pi".into())
    }
    /// pi on one account's launch view: every `claude` it starts uses
    /// `view` and the real Claude binary.
    pub(super) fn command(self, provider: Provider, view: &Path) -> Command {
        let claude = claude_binary();
        let mut cmd = Command::new(self.binary());
        cmd.env(provider.env_var(), view);
        cmd.env(provider.bin_var(), &claude);
        cmd.env(PROVIDER_PATH, &claude);
        provider::scrub(&mut cmd);
        // SAFETY: umask is async-signal-safe; it changes only the forked child.
        unsafe {
            cmd.pre_exec(|| {
                libc::umask(0o077);
                Ok(())
            });
        }
        cmd
    }
}
/// The Claude binary pi's provider should run: `ASWAP_CLAUDE_BIN`, else the
/// provider's own `PI_CLAUDE_CODE_PROVIDER_PATH`, else `claude` on PATH.
fn claude_binary() -> OsString {
    [Provider::Claude.bin_var(), PROVIDER_PATH]
        .iter()
        .find_map(|key| std::env::var_os(key).filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "claude".into())
}
/// A POSIX shell word for `value`.
fn quote(value: &OsStr) -> Vec<u8> {
    let mut out = b"'".to_vec();
    for &b in value.as_bytes() {
        if b == b'\'' {
            out.extend_from_slice(b"'\\''");
        } else {
            out.push(b);
        }
    }
    out.push(b'\'');
    out
}
/// The shim pi's provider runs as `claude`: one `aswap run claude --pick`
/// per call, with the real Claude binary.
pub(super) fn shim_text(aswap: &Path, claude: &OsStr) -> Vec<u8> {
    let mut text = b"#!/bin/sh\n# Written by aswap run claude --via pi: one account per claude call.\nexport ASWAP_CLAUDE_BIN=".to_vec();
    text.extend(quote(claude));
    text.extend_from_slice(b"\nexec ");
    text.extend(quote(aswap.as_os_str()));
    text.extend_from_slice(b" run claude --pick -- \"$@\"\n");
    text
}
/// Write the shim under the store (0700), replacing it atomically so a pi
/// already running never reads half of it.
fn shim(store: &Store) -> Result<PathBuf> {
    let dir = store.root.join("bin");
    store::private_dir(&dir)?;
    let path = dir.join("claude-pick");
    let aswap = std::env::current_exe().safe("cannot locate the aswap binary")?;
    let text = shim_text(&aswap, &claude_binary());
    if std::fs::read(&path).is_ok_and(|old| old == text) {
        return Ok(path);
    }
    let mut temp = tempfile::Builder::new()
        .prefix(".claude-pick-")
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempfile_in(&dir)
        .safe("cannot write the claude shim")?;
    temp.write_all(&text).safe("cannot write the claude shim")?;
    temp.persist(&path).safe("cannot write the claude shim")?;
    Ok(path)
}
/// pi with an account chosen per `claude` call. No lease and no view here:
/// each call holds its own while it runs.
pub(super) fn per_call(
    store: &Store,
    provider: Provider,
    via: Via,
    args: &[String],
) -> Result<i32> {
    via.check(provider)?;
    let shim = shim(store)?;
    let mut cmd = Command::new(via.binary());
    cmd.env(PROVIDER_PATH, &shim);
    provider::scrub(&mut cmd);
    let status = cmd
        .args(args)
        .status()
        .safe("cannot launch pi; is it on PATH (or ASWAP_PI_BIN)?")?;
    Ok(status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(1)))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn the_shim_quotes_paths_and_passes_every_argument() {
        let text = shim_text(
            Path::new("/opt/it's aswap/bin/aswap"),
            OsStr::new("/nix/store/x/claude"),
        );
        let text = String::from_utf8(text).unwrap();
        assert!(text.starts_with("#!/bin/sh\n"));
        assert!(text.contains("export ASWAP_CLAUDE_BIN='/nix/store/x/claude'\n"));
        assert!(
            text.ends_with("exec '/opt/it'\\''s aswap/bin/aswap' run claude --pick -- \"$@\"\n")
        );
    }
    #[test]
    fn only_claude_can_be_driven_by_pi() {
        assert!(Via::Pi.check(Provider::Claude).is_ok());
        assert!(Via::Pi.check(Provider::Codex).is_err());
    }
}
