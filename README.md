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
shared project history. Any number of plain or auto sessions can use the same or different accounts
concurrently. Starting another session never waits for an account to become free.

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
`$XDG_DATA_HOME/aswap/migration-backups/`. Legacy refresh-capable runs must
finish before their histories migrate; simultaneous new launches serialize migration.
Anonymous legacy auto directories migrate only when all provider profiles are
idle. No transcript text is logged or used to infer session identity.

Background OAuth refresh is **opt-in on one owner**: `aswap sync --owner`.
Each active local run can refresh its non-follower store through aswap. Every
launch view is access-only; native sessions never rotate copied refresh tokens.
Without background ownership configured, inactive usage works while access tokens remain valid;
status JSON reports `credential_mode: "refresh-disabled"`. For three machines,
configure the owner/followers below before restarting monitors and resuming sessions. Re-login on the
owner if the provider has already invalidated a refresh token. API-key accounts
can run but may not expose quota.


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

Auth-isolated `run` and selection-only `switch` avoid default-login writes.
Parallel launches share provider state while keeping account authentication private.

## Quota and autoswitch

```sh
aswap auto claude threshold 90
aswap auto claude prefer weekly-reset
aswap auto claude on
aswap auto claude once
aswap auto claude watch
aswap auto claude off
aswap auto codex on
aswap auto codex watch
```

`on/off/threshold/prefer` persist policy; `watch` runs a foreground selection monitor
and `once` evaluates one tick. Selection monitors affect future launches.
Add `--activate-live` to `once` or `watch` only when default-file activation is
wanted. `off` stops selection changes and avoids usage requests by the monitor.
It does not terminate a separately running `run --auto` session.

```sh
aswap run claude --account work --auto
aswap run codex --account personal --auto
```

`run --auto` supervises the provider in a PTY and forwards its arguments normally.
The provider binary is `claude`/`codex` from `PATH`, or `ASWAP_CLAUDE_BIN`/
`ASWAP_CODEX_BIN` when set, which lets a `claude` wrapper on `PATH` run
`aswap run claude --auto -- "$@"` without recursing:

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
the restart. Claude’s PID registry can identify session selection inside the
provider, even when subagents open other transcripts. A known Claude UUID survives
unrelated shared history writes. A fork must establish its new identity first.

Candidates need fresh known quota below the threshold, ten points of improvement
and a 300-second cooldown; a recognized limit bypasses the latter two gates.
`prefer headroom` (the default) picks the candidate with the lowest usage;
`prefer weekly-reset` picks the one whose weekly window resets soonest, so
quota that would expire unused goes first. With `weekly-reset`, new launches and
the selection monitor also move to an eligible account whose weekly reset is
strictly sooner, even below the threshold; running sessions switch only after an
explicit provider limit or authentication failure. Quota polling never interrupts
active tools. Failure detection matches diagnostic line shapes rather than
arbitrary conversation prose. A recalled line identical to a live diagnostic
cannot be distinguished from it in raw terminal output.
Resume identity and arguments are checked before stopping the child;
if they are unavailable, the current provider stays running.
Switches stop the child with SIGTERM, wait five seconds, then use SIGKILL if
needed. The exact same shared session resumes on the new account. Clean exits do
not restart or send a follow-up. Rejected accounts and known identity aliases are
excluded for the rest of that run, even if an owner replaces their usage caches.
Restarts are capped at eight. Runtime auth/config
views stay private under `$XDG_DATA_HOME/aswap/runtimes/`; history stays shared.

Claude usage comes from the OAuth usage endpoint; Codex uses the official
[app-server rate-limit RPC](https://learn.chatgpt.com/docs/app-server), with
provider-reported durations and reset timestamps.

`status` groups profiles by provider under that provider's auto policy, marks
the active one with `●`, and shows each window (session, weekly and any exposed
Claude model weekly windows) with an aligned bar, percent used and time until
reset. Observations older than ten minutes carry their age and dim; a failed
check says what went wrong and, for an expired login, how to re-add it. Logins
broken for over a day collapse to a one-line last-known summary. `list` is a
compact offline view with one quota line per profile. The single
`remaining_percent` JSON/Waybar metric remains the most restrictive window for
autoswitch compatibility, not specifically session headroom. Aliases of one
login share a fresh observation instead of each polling the provider.
`status` refreshes all profiles through the cache/backoff policy.

Email and tier hints come from each stored profile's own config/auth claims and
appear only in human terminal output. `NO_COLOR` disables color; redirected
output has no ANSI escapes, and narrow terminals wrap the view. Unknown identity
or tier is explicit. `add` warns and `list` flags duplicate account identities
using an in-memory SHA-256 fingerprint of stable account/organization IDs (email
fallback), never a raw-token comparison. Unknown identities cannot be declared
duplicates. A second label still captures the current login; log into the other
account before adding it. Token rotation does not make a new account identity.

All usage surfaces share a locked, persisted 180-second success cache. Failures
back off from 60 seconds exponentially to at most 15 minutes; a longer
`Retry-After` is honored. An error, stale observation (over ten minutes)
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

Usage failures include a safe reason and HTTP status where available: 401
expired/invalid token, 403 forbidden, 429 rate limited (with `Retry-After`), network
failure, or a Codex app-server start/protocol/timeout error. JSON exposes
`usage_error`, `http_status`, `retry_after_seconds`, `last_attempt` and
`next_poll_at`. Response bodies and child stderr are never included. A stale bar
retains the last-known utilization with a `(stale)` tag (dimmed with color).
Passed reset times trigger a fetch immediately, once per reset before ordinary
failure backoff applies again.

```sh
aswap usage --refresh                 # all profiles; explicit backoff override
aswap usage claude --account work --refresh --json
```

`--refresh` bypasses quota TTL/backoff, including Retry-After, for that explicit
request. It does not force an unnecessary OAuth rotation. On a follower, the
request goes to the owner instead of polling the provider locally.

## Three-machine ownership and sync

Use one always-available SSH-accessible machine as the refresh/usage owner. The
other two are access-only followers; all three can still launch account profiles.
Stop the monitors and finish/stop existing aswap sessions while assigning roles;
then re-login to each account on the
chosen owner if existing copied refresh tokens are already invalid. Keep labels
consistent across machines; conflicting known account identities are refused.

```sh
# On the server (or chosen owner):
aswap sync --owner
# On the laptop and desktop; synthetic-owner is an SSH Host alias:
aswap sync synthetic-owner
# To pull again explicitly:
aswap sync
# To disable background refresh/sync without deleting profiles:
aswap sync --off
```

Configure SSH key authentication and known host keys normally. `aswap` must be on
the owner's noninteractive SSH PATH and use the same XDG data-store environment
as its local services. Sync uses a bounded noninteractive SSH pipe; ordinary
terminal/JSON output reports counts only. It transfers access-only auth,
account-bound metadata and quota caches, never transcripts, settings, or refresh
tokens. The hidden transport command is for SSH pipes, not human use.

Only the owner polls provider quota and refreshes canonical credentials for the
three-machine setup. Every monitor/client shares its persisted per-profile cache;
followers pull at most once per 30 seconds. A refresh revokes the old access
token at once, so the owner also pushes: one process per follower (the monitor or
any run) keeps an SSH stream (`sync --serve --watch`) open, and the owner sends a
packet within a quarter second of any profile auth change plus a heartbeat every
20 seconds; the 30-second pull remains the fallback. Every live view, including
owner runs, notices a changed profile token on its next 250ms tick and installs
it, so a rotation reaches running sessions in about a second. An identity lock serializes
refresh; waiters re-read the refreshed credentials, and duplicate aliases receive
the same rotation. Failed refresh attempts share a cooldown rather than creating
one request per live session. Stale runtime writes cannot overwrite canonical auth.

Live sessions have individual PID/start-time/boot records. Rename/remove refuse
while any session uses the label; dead/reused PIDs and killed-session records are
collected automatically. Killing a supervisor also stops its provider child.
Automatic switching remains local to each session and can choose an account
already used by any number of other sessions. Restart sessions from older aswap
releases during deployment so they also receive the new access-only views.

Use `aswap run` on followers; independently running the native CLI against old
refresh-capable copies is outside this ownership protocol. Perform login and
profile administration on the owner, then sync while affected profiles are idle.
Session guards are local to each machine's store; coordinate label removals and
renames with remote users before administering the owner.
Unmatched local profiles retain their files but become ineligible for auto quota
selection. On owner/network failure, last-known quotas become unavailable/stale;
followers never promote themselves or guess which machine has a valid refresh
token. Existing sessions keep their current access token until it expires; a
provider version that cannot reload an updated token may require an explicit
resume after reconnection. Keep all machines' clocks synchronized normally.

Home Manager can apply roles when monitor services start:

```nix
# Owner:
programs.aswap.refreshOwner = true;
# Followers, instead:
programs.aswap.syncHost = "synthetic-owner";
```

For a follower that must import only specified profiles, declare an exact
provider/label allowlist on that machine:

```nix
programs.aswap.syncProfiles = {
  allow = [ "claude:personal" "codex:personal" ];
  deny = [];
};
```

This writes credential-free `$XDG_CONFIG_HOME/aswap/sync-policy.json` (normally
`~/.config/aswap/sync-policy.json`) even without an enabled monitor. Manual
configuration uses the same JSON shape: `{"allow":["claude:personal"],"deny":[]}`.
Keys are case-sensitive `claude:LABEL` or `codex:LABEL`; wildcards and identity or
email matching are unsupported. `allow = null` permits all imports, `allow = []`
permits none, and deny always wins. `syncProfiles = null` manages no policy file;
an absent file allows all imports. Use an explicit allowlist for a non-work-only
machine so new or renamed owner labels stay excluded until reviewed.

Both polling and streaming sync reread this policy for every received packet,
including stale packets; role changes never overwrite it. Excluded remote profiles
are not installed or checked for auth/identity conflicts. Malformed, unreadable or
oversized policy files fail closed with a fixed diagnostic that contains no file
contents. The policy limits local imports; the owner still transmits its complete
access-only packet over SSH, and local `add`, `switch`, and `run` remain available.

**Existing excluded profiles block sync.** Nothing is hidden, pruned, or stopped.
Before applying a restrictive policy, finish affected sessions and explicitly
remove excluded local profiles using `aswap remove PROVIDER LABEL`. A busy excluded
profile cannot be removed: it remains on the machine and the non-work-only
requirement is **not yet satisfied** until its sessions finish and guarded cleanup
succeeds. Until then all sync imports fail with `sync: excluded-local-profile`,
leaving profile auth, caches, defaults and account metadata unchanged. Sync
scheduling/failure counters may still update. No refresh ownership is transferred.

`refreshOwner = null` (default) preserves CLI configuration. Explicit `false`
with no `syncHost` disables automatic refresh; owner and follower options are
mutually exclusive. Enable the desired `autoswitch.<provider>.enable` services on
all machines after roles are assigned. No remote login or account-store operation
is run during development; tests use synthetic HTTP outcomes and an SSH stub.

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
    autoswitch.claude = { enable = true; threshold = 90; prefer = "weekly-reset"; };
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

Codex quota windows are named by their reported duration, including weekly-only
plans with no session window. Reset countdowns include days. When the app-server
exposes them, status/list and usage JSON include credit balance and available
usage limit resets. Terminal credit balances round to integers with thousands
separators; JSON retains full precision. Unknown automatic reload settings and
zero/unknown reset counts are hidden. The current Codex RPC does not expose
reload settings; credit balance does not imply reload is on.

If follower setup fails, sync now prints a safe error class, the remote exit code
and a classified first stderr line. Unknown remote text remains fully redacted.
`owner-not-configured` means the store selected by the SSH command is not currently
an owner, even if a service announced ownership earlier. On the intended owner,
run `aswap sync --owner` using the same user/store as its monitor, then retry the
follower. Profile runs must be idle when changing roles. Verify XDG_DATA_HOME if
SSH and the service disagree. Sync never promotes a remote host automatically.
A configured owner refuses `sync HOST`; use `sync --off` first for an intentional
role change. Duplicate logins under distinct labels produce a count-only warning
and are retained; a label mapped to a different identity still requires alignment.

### Expired logins and the account in each terminal

`aswap sessions` shows each locally running session's provider, actual account
label, Herdr terminal (when available), and supervisor/child PID. Use
`aswap sessions --json` for scripts. `aswap list` shows saved account identity
hints; the selected default only controls future launches. New runs print their
account at launch and after automatic switches. Older supervisors cannot mark a
retained history lease separately, so a session started before this update may
appear under both its original and current account after a switch.

Automatic runs recognize explicit expired-login/HTTP 401 errors as well as
quota errors, and resume the exact tracked conversation on another eligible
account. The rejected profile is marked authentication-unavailable, preserving
its last-known quota. Automatic restart still requires an exact conversation ID;
if none was created, restart the CLI manually. If no healthy account is known,
the current UI stays open so you can renew the login.

Refresh tokens are normally renewed automatically. A revoked/expired refresh
login needs the provider's interactive login again. In an owner/follower setup,
do this on the **refresh owner**, not a follower whose next sync replaces local
credentials. Stop runs using the affected profile before replacing it:

```sh
# On the refresh owner, from a normal shell outside any managed run:
claude auth login                  # sign into the account shown by aswap list
aswap remove claude work           # refuses while this profile is running
aswap add claude work
aswap status claude
# On each follower:
aswap sync
```

Replace `work` with the affected label. This removes/recreates the stored login;
shared conversation history remains. For Codex use `codex login` and substitute
`codex` in the profile commands. Keep the old profile until interactive login
succeeds; never remove it first. Check the identity shown after re-adding.

SQLite state migrated into a shared home or snapshotted from a managed entry
uses SQLite's online backup API, including committed WAL pages. Transient WAL,
SHM and rollback-journal sidecars are not copied independently. Snapshots are
validated before publication; corrupt sources are retained and rejected, and
busy backups have a bounded deadline. Existing destinations remain untouched.

### Model-compatible Codex failover

Automatic Codex runs keep the requested model when changing accounts. The model
comes from `-m`/`--model` (including `--model=...`), or Codex's effective
configuration, including `-c` overrides and a selected named profile. A model
resolved from configuration is pinned on launch and exact-session resume.

Before selecting another account, aswap queries that account's `model/list`
catalog, including hidden models and subsequent pages, using an isolated,
access-only login. It does not infer model access from labels or plan names.
Catalog results are cached for five minutes within the running supervisor and
invalidated when credentials change. Missing, failed, or incomplete catalogs
exclude that candidate. A provider's explicit unsupported-model error excludes
the current account for the rest of this run without marking its quota spent.

When no compatible account has verified quota below the threshold, aswap prints
`no eligible account with verified support for model ... and available quota`.
An open provider stays open; a provider that has already exited returns failure.
The launcher never substitutes another model. The initial explicitly selected
account remains usable even if its catalog cannot be queried; every automatic
switch target must pass verification. Restart the supervisor after changing
model selection inside the provider UI so aswap uses the new required model.
