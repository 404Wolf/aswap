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

## Accounts and isolated sessions

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

`switch` selects the account for future `aswap run` invocations. `run` sets
`CLAUDE_CONFIG_DIR` or `CODEX_HOME` to that account's private profile. Different
accounts can run concurrently, including two accounts from the same provider.
The same account profile has an exclusive lease: another launch, deletion or
activation is refused while that profile is running. Provider exit codes are
propagated. The first captured account becomes the default for that provider.

Profiles import credentials and minimal identity/onboarding metadata only.
They do not import settings, plugins, MCP servers or past transcripts. Configure
the isolated profile through the official CLI if needed. History stays with
the profile, so native resume works for conversations started there.
Inherited API-key and authentication overrides are removed from provider
processes so they do not silently select another account. Codex is launched
with its credential store explicitly set to `file`.

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

`run --auto` explicitly supervises a new interactive session in a PTY. It can
start with a healthier account, then switches when a known quota reaches the
threshold or the CLI prints a recognized 429/limit error. It requires an
eligible stored alternate. Candidates must have known, fresh quota below the
threshold; the best remaining headroom wins. Ordinary threshold switches need
a ten-point improvement and a 300-second cooldown; a provider limit bypasses
those two gates. Unknown quota is never treated as free capacity.

A switch stops the old child (SIGTERM, five-second grace, then SIGKILL), changes
the runtime credentials and invokes `claude --continue` or `codex resume --last`
in the same private history directory. It never copies conversation history
between accounts. Resume depends on the provider having persisted the session;
the configurable follow-up prompt continues interrupted work after a forced
switch, while `resume-prompt off` leaves the next prompt to you. Limit-text
detection is heuristic; quota polling is the primary signal. Restarts are capped
at eight per invocation. Provider arguments are refused with `--auto`, so
scripts and explicit command arguments cannot be replayed automatically.

Auto histories remain under
`$XDG_DATA_HOME/aswap/sessions/<provider>/<initial-label>/run-*` after exit.
Older anonymous `sessions/run-*` histories remain usable at their original paths.
To resume one later, point the official CLI's config-home environment variable
at that session directory. It contains private credentials and transcripts;
treat it as sensitive. The source account profile receives refreshed auth on
normal exit and during observation. Abrupt termination can leave the freshest
auth only in the retained session directory.

Claude usage reads five-hour and seven-day windows from its OAuth usage
endpoint, including supported model windows. Codex usage uses the official
[app-server rate-limit RPC](https://learn.chatgpt.com/docs/app-server): primary
and secondary windows of the standard `codex` bucket, with provider-reported
durations/reset timestamps. These need not be five-hour/weekly on every plan.
Additional Codex model buckets are not combined into the selection metric.
No terminal status scraping or speculative OpenAI usage endpoint is used.

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

Both installed CLIs support an initial resume prompt: `claude --continue --
"<prompt>"` and `codex resume --last -- "<prompt>"`. Prompts are literal arguments;
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
