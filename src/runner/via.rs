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
    /// Whether this program can drive `provider`.
    pub fn check(self, provider: Provider) -> std::result::Result<(), ViaError> {
        match (self, provider) {
            (Self::Pi, Provider::Claude) => Ok(()),
            _ => Err(ViaError::Provider),
        }
    }
    fn binary(self) -> OsString {
        Provider::Pi.binary()
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
/// What can stop a `--via` launch.
#[derive(Debug, thiserror::Error)]
pub enum ViaError {
    #[error("--via pi drives Claude accounts only")]
    Provider,
    #[error("a path for the claude shim cannot be quoted for /bin/sh")]
    Unquotable,
}
impl From<ViaError> for Error {
    fn from(e: ViaError) -> Self {
        Error(match e {
            ViaError::Provider => "--via pi drives Claude accounts only",
            ViaError::Unquotable => "a path for the claude shim cannot be quoted for /bin/sh",
        })
    }
}
/// The shim pi's provider runs as `claude`: one `aswap run claude --pick`
/// per call, with the real Claude binary.
pub(super) fn shim_text(aswap: &Path, claude: &OsStr) -> std::result::Result<Vec<u8>, ViaError> {
    let quote = |word: &OsStr| {
        shlex::bytes::try_quote(word.as_bytes())
            .map(|q| q.into_owned())
            .map_err(|_| ViaError::Unquotable)
    };
    let mut text = b"#!/bin/sh\n# Written by aswap run claude --via pi: one account per claude call.\nexport ASWAP_CLAUDE_BIN=".to_vec();
    text.extend(quote(claude)?);
    text.extend_from_slice(b"\nexec ");
    text.extend(quote(aswap.as_os_str())?);
    text.extend_from_slice(b" run claude --pick -- \"$@\"\n");
    Ok(text)
}
/// Write the shim under the store (0700), replacing it atomically so a pi
/// already running never reads half of it.
fn shim(store: &Store) -> Result<PathBuf> {
    let dir = store.root.join("bin");
    store::private_dir(&dir)?;
    let path = dir.join("claude-pick");
    let aswap = std::env::current_exe().safe("cannot locate the aswap binary")?;
    let text = shim_text(&aswap, &claude_binary())?;
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
        let aswap = Path::new("/opt/it's aswap/bin/aswap");
        let claude = OsStr::new("/nix/store/x y/claude");
        let text = String::from_utf8(shim_text(aswap, claude).unwrap()).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines[0], "#!/bin/sh");
        let export = shlex::split(lines[2]).unwrap();
        assert_eq!(export, ["export", "ASWAP_CLAUDE_BIN=/nix/store/x y/claude"]);
        let exec = shlex::split(lines[3]).unwrap();
        assert_eq!(
            exec,
            [
                "exec",
                "/opt/it's aswap/bin/aswap",
                "run",
                "claude",
                "--pick",
                "--",
                "$@"
            ]
        );
        assert!(lines[3].ends_with(" -- \"$@\""));
        assert!(shim_text(Path::new("/a\0b"), claude).is_err());
    }
    #[test]
    fn only_claude_can_be_driven_by_pi() {
        assert!(Via::Pi.check(Provider::Claude).is_ok());
        assert!(Via::Pi.check(Provider::Codex).is_err());
        assert!(Via::Pi.check(Provider::Pi).is_err());
    }
}
