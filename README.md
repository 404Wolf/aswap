# aswap

Private account profiles for **Claude Code** and **Codex CLI**, written in Rust
for Linux. A port of the command-line account-switching core of
[claude-swap](https://github.com/realiti4/claude-swap), extended with Codex,
Waybar and Home Manager integration. Original work © 2026 Onur Cetinkol,
MIT licensed; see [LICENSE](LICENSE), [NOTICE](NOTICE) and [DESIGN.md](DESIGN.md).

## Install

From this checkout:

```sh
nix develop                 # installs mandatory pre-commit/pre-push scanning hooks
nix build                  # result/bin/aswap
nix profile add path:.     # optional installation
```

Or, with a recent stable Rust toolchain:

```sh
cargo install --path . --locked
```

The official `claude` and/or `codex` executable must be on PATH for `run`.
Codex usage also requires a version supporting the app-server
`account/rateLimits/read` interface. aswap does not install either provider.
File-based authentication is required; macOS Keychain, OS keyrings, the
reference TUI/menu bar, and Windows are outside this release's scope.

## Accounts and shared sessions

Log in using the official CLI, then capture that file-based login. Labels use
1–48 ASCII letters, digits, hyphens or underscores; use a label such as `work`,
rather than an email address. Run the provider's own login flow again before
capturing a second account.

```sh
aswap add claude work
aswap add codex personal
aswap list
aswap list --json
aswap switch claude work
aswap status
aswap usage codex --account personal --json
aswap run claude --account work
aswap run codex --account personal -- --help
aswap run claude --account work -- --resume
aswap run codex --account personal -- resume --last
aswap rename claude work office
aswap remove codex personal
```

`switch` selects the account for future runs. Every launch inherits the caller's
working directory unchanged. Auth is private, while state comes from the caller's
`CLAUDE_CONFIG_DIR` / `CODEX_HOME` (or `~/.claude` / `~/.codex`). Conversations,
project trust, MCP configuration, settings, instructions, plugins and skills are
available across accounts, like one provider installation. `/resume` sees the
shared project history. Separate accounts may run concurrently.

Each launch uses a private view under the aswap store. Ordinary state entries
are per-entry symlinks into the shared home. Home Manager-managed symlinks are
snapshotted locally instead: aswap never replaces or writes through them, and
provider edits to those snapshots are not published back. Claude account keys
are overlaid onto a copy of the global config; non-account changes (including
nested project trust/MCP state) merge back under the store lock, preserving
concurrent changes. The default home's account identity/auth is untouched.
Codex shares configuration and session state, with file-based auth forced private.

On first launch after upgrading, isolated profile and retained auto histories
are merged into the shared home with **new files only**. Existing files and
session indexes are never overwritten, and migration never follows destination
symlinks. Originals, including collisions, are retained privately under
`$XDG_DATA_HOME/aswap/migration-backups/`. Busy profiles migrate when idle.
Anonymous legacy auto directories migrate only when all provider profiles are
idle. No transcript text is logged or used to infer session identity.

To refresh an expired login, launch its profile and use Claude's interactive
`/login`, or `aswap run codex --account personal -- login`. The official CLI
owns OAuth refresh; aswap does not perform its own refresh-token exchange.
An inactive expired Claude login may report unknown usage until it is launched
or logged in again. API-key Codex profiles can run but may not expose quota.

### Explicit default-login activation

```sh
aswap switch claude work --activate-live
```

This additionally swaps the current provider credential file. It follows the
caller's `CLAUDE_CONFIG_DIR`/`CODEX_HOME`, falling back to the standard provider
home. Claude's account identity is merged into its global config without
replacing unrelated settings. Symlinks (including Home Manager managed files)
and unreadable config are refused. The outgoing login is preserved privately
under `live-backups/`, and recognizable rotated credentials are synchronized
back to their matching stored profile. An existing unrelated provider process
is not restarted; its credential reload behavior is provider-specific.

Normal isolated `run` and selection-only `switch` avoid default-login writes.
Prefer isolated sessions for parallel accounts.

## Quota and autoswitch

```sh
aswap auto claude threshold 90
aswap auto claude on
aswap auto claude once
aswap auto claude watch
aswap auto claude off
aswap auto codex on
aswap auto codex watch
```

`on/off/threshold` persist policy; `watch` runs a foreground selection monitor
and `once` evaluates one tick. Selection monitors affect future launches.
Add `--activate-live` to `once` or `watch` only when default-file activation is
wanted. `off` stops selection changes and avoids usage requests by the monitor.
It does not terminate a separately running `run --auto` session.

```sh
aswap run claude --account work --auto
aswap run codex --account personal --auto
```

`run --auto` supervises the provider in a PTY and forwards its arguments normally:

```sh
aswap run claude --auto -- --resume <session-uuid> --dangerously-skip-permissions
aswap run claude --auto -- --continue --model sonnet
aswap run codex --auto -- resume <session-uuid> --model <model>
```

On a forced switch, session selectors are replaced by `claude --resume <id>` or
`codex resume <id>` and the configured follow-up. All other provider options are
kept verbatim. An initial positional prompt, Claude `-p`/`--print`, and piped
stdin are one-shot: they are omitted on relaunch with a generic note, never echoed
or replayed. Interactive terminal input continues normally. Provider option arity
is described by the CLI help when needed; unknown ambiguous option values cause a
safe refusal to restart rather than risking prompt replay. Use `--` before a
literal positional prompt to make its boundary explicit.

Session IDs come from an explicit UUID, a preallocated Claude `--session-id`,
Claude's own PID registry, or transcript filenames held open by that child/process
tree. Transcript contents are not read. The tracker never picks the globally
newest conversation: if identity cannot be established unambiguously, it refuses
the restart. Session selection inside the provider can be detected through its
open transcript descriptors; ambiguous provider versions fail safely.

Candidates need fresh known quota below the threshold, ten points of improvement
and a 300-second cooldown; a recognized limit bypasses the latter two gates.
Switches stop the child with SIGTERM, wait five seconds, then use SIGKILL if
needed. The exact same shared session resumes on the new account. Clean exits do
not restart or send a follow-up. Restarts are capped at eight. Runtime auth/config
views stay private under `$XDG_DATA_HOME/aswap/runtimes/`; history stays shared.

Claude usage comes from the OAuth usage endpoint; Codex uses the official
[app-server rate-limit RPC](https://learn.chatgpt.com/docs/app-server), with
provider-reported durations and reset timestamps.

`status` shows every managed profile, marks the active one and displays SESSION
(5h), WEEKLY and any exposed Claude model weekly windows separately. Each window
has percent used, a progress bar and time until reset. `list` is a compact offline
view of the same windows. Both label cached/stale/backoff observations; missing
quota stays unknown. The single `remaining_percent` JSON/Waybar metric remains the
most restrictive window for autoswitch compatibility, not specifically session
headroom. `status` refreshes all profiles through the cache/backoff policy.

Email and tier hints come from each stored profile's own config/auth claims and
appear only in human terminal output. `NO_COLOR` disables color; redirected
output has no ANSI escapes, and narrow terminals wrap the view. Unknown identity
or tier is explicit. `add` warns and `list` flags duplicate account identities
using an in-memory SHA-256 fingerprint of stable account/organization IDs (email
fallback), never a raw-token comparison. Unknown identities cannot be declared
duplicates. A second label still captures the current login; log into the other
account before adding it. Token rotation does not make a new account identity.

All usage surfaces share a locked, persisted 180-second success cache. Failures
back off exponentially from 600 seconds up to one hour, or longer when required
by `Retry-After` plus a margin. An error, stale observation (over ten minutes)
or elapsed reset makes remaining quota unknown. Cache hits never bypass a
backoff. `list` is offline; `status`, `usage`, `waybar` and active monitors
fetch only when due. The HTTP/RPC clients have bounded timeouts and response
sizes; provider error bodies are redacted.

`aswap rename <provider> <old> <new>` preserves active selection, provider-wide
auto policy, cached quota and retained histories. It validates labels and refuses
a running profile or an existing destination. Renames run under the store lock
with recovery after an interrupted operation. Profile-owned histories move with
the label; anonymous legacy histories have no label to rename and remain intact.
`remove` identifies duplicate logins by their other managed labels. Both commands
support `--json`; no email or credential data is included in their output.

After a forced `run --auto` switch, the default follow-up is:
"You were interrupted by a usage-limit account switch. Continue exactly where you
left off." A successful session exit sends no follow-up. Configure or disable it:

```sh
aswap auto claude resume-prompt 'Continue the interrupted task.'
aswap auto codex resume-prompt off
```

Both CLIs support a prompt with an exact resume ID: `claude --resume <id> --
"<prompt>"` and `codex resume <id> -- "<prompt>"`. Prompts are literal arguments;
previous user commands are not replayed. `off` keeps native resume without sending
a follow-up. In Home Manager, set
`programs.aswap.autoswitch.<provider>.resumePrompt` to text or `null` (off); the
monitor service applies it alongside the threshold when enabled.

## Waybar

```json
{
  "modules-right": ["custom/aswap"],
  "custom/aswap": {
    "exec": "aswap waybar",
    "return-type": "json",
    "restart-interval": 30,
    "escape": true
  }
}
```

The command streams one JSON object per line, showing active labels and
remaining quota (`?` when unknown). Display cadence defaults to 30 seconds
(minimum 30); network refresh still respects the shared cache/backoff. Use
`aswap waybar --once` for a snapshot. No Waybar `interval` is needed for the
streaming command.

## Home Manager

Use this checkout as a flake input, then import its module:

```nix
{
  inputs.aswap.url = "path:/absolute/path/to/checkout";
  # In your Home Manager configuration:
  # imports = [ inputs.aswap.homeManagerModules.default ];
}
```

```nix
{
  imports = [ inputs.aswap.homeManagerModules.default ];
  programs.aswap = {
    enable = true;
    # package = inputs.aswap.packages.${pkgs.stdenv.hostPlatform.system}.default;
    autoswitch.claude = { enable = true; threshold = 90; };
    autoswitch.codex = { enable = true; threshold = 90; };
    waybar.enable = true;
  };
  programs.waybar.settings.mainBar.modules-right = [ "custom/aswap" ];
}
```

This installs aswap and optional `aswap-claude`/`aswap-codex` user services.
Services initialize policy at startup and monitor selections with a private
umask and a 60-second failure-restart delay. `waybar.enable` adds the module
configuration under `mainBar`; choose where to place it yourself. The module
does not create or modify files in `~/.claude` or `~/.codex`, and services never
request `--activate-live`.

## Security notes and development

Account data lives outside Git at `$XDG_DATA_HOME/aswap/`, defaulting to
`$HOME/.local/share/aswap/`. Directories are 0700; aswap-created files are 0600;
provider children receive umask 0077. Writes use same-directory temporary
files, fsync and atomic replacement; advisory locks serialize store/cache
updates. Labels are path-safe. Symlinks and stores inside a Git checkout are
refused. This is private plaintext storage, not encryption or an OS keychain.
Protect the data directory and its backups accordingly.

aswap's terminal output contains labels, local account email/plan hints and quota, never credential bytes. Emails and identity hashes are excluded from JSON, Waybar, caches and errors. Errors omit
private JSON, server bodies and paths. Interactive provider output passes
through to your terminal; it is not persisted by aswap. Providers may keep their
own logs/transcripts in the isolated profile. Do not paste those into issues.

Development uses **only synthetic fixtures**. Never point tests at real logins:

```sh
nix develop
scripts/isolated cargo test --locked
scripts/isolated cargo clippy --all-targets --locked -- -D warnings
cargo fmt --all --check
scripts/test-scanning
nix flake check
scripts/scan-release
```

The isolation wrapper replaces HOME, XDG paths, CLAUDE_CONFIG_DIR and CODEX_HOME
with temporary directories. Integration tests also sandbox their subprocesses
independently; fake executables simulate provider behavior without network
access. No live account was used to validate this release. Nix exports Linux
x86_64 and aarch64 packages; local build verification covers x86_64 Linux.

`nix develop` installs repository pre-commit and pre-push hooks. They fail
closed without gitleaks; never bypass them. CI checks history and shipping
files, plus Rust tests, clippy, formatting and Home Manager evaluation. The
release scanner always scans all Git history; it also scans
`origin/main..HEAD` when available, or the complete shipping tree on the first
push. Ignored compiler output is excluded from that tree; force-added build
artifacts, credential filenames and runtime-state paths are rejected by a
separate index guard. The scanner prints `git status` and `git ls-files` for
manual review. Passing a scan is not authorization to publish: review the
full-history result and file list, then obtain the operator's approval before
any push. This project setup does not create a hosted repository or push.

## Provider terms

Using multiple accounts to avoid usage limits may violate provider terms.
Users are responsible for their account use and compliance with applicable
terms. aswap is independent of Anthropic and OpenAI; provider interfaces may
change without notice.
