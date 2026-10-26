# aswap design

## M0: reference findings

Source reviewed: realiti4/claude-swap, local reference checkout supplied in
GOALS.md, especially paths.py, credentials.py, oauth.py, switcher.py,
autoswitch.py, poll_policy.py, session.py, settings.py and pace.py.
claude-swap is MIT, copyright 2026 Onur Cetinkol; its notice must be retained.
No user credentials or reference runtime stores were inspected.

The reference provides add/list/switch/remove, usage, best/next-available
selection, auto monitoring, aliases and account policies, mappings, isolated
parallel sessions, JSON output, TUI and macOS menu bar. aswap targets the
Linux command-line subset specified in GOALS.md, with a Waybar stream instead
of the reference's platform-specific UI.

On Linux its store is XDG_DATA_HOME/claude-swap (default
~/.local/share/claude-swap); legacy macOS/Windows storage is
~/.claude-swap-backup. It separates configs, credentials, sessions, mappings,
settings and cache/usage.json, with advisory locks and atomic private writes.
Claude credentials resolve to CLAUDE_CONFIG_DIR/.credentials.json or
~/.claude/.credentials.json. Global config uses legacy .config.json when
applicable, otherwise CLAUDE_CONFIG_DIR/.claude.json or ~/.claude.json.
Default switching saves the old credentials/config, replaces credentials and
account identity while preserving unrelated global config. This is a file
swap, not an environment change. Running Claude processes can pick it up;
it does not normally kill and restart every session.

Usage is GET https://api.anthropic.com/api/oauth/usage with Authorization:
Bearer, anthropic-beta: oauth-2025-04-20, User-Agent: claude-swap/1.0 and a
5-second timeout. It returns five_hour and seven_day utilization/reset values,
plus optional model windows. The profile endpoint is /api/oauth/profile;
refresh uses https://platform.claude.com/v1/oauth/token. These are provider
interfaces, not a stability guarantee. The reference caches successful usage,
coordinates polls, distinguishes stale/unknown from zero, honors Retry-After
and backs off rather than burning quota probing a saturated usage endpoint.
Normal freshness floor is 180s, active cadence up to 300s, idle/exhausted up
to 600s; bounded urgent mode is 60s. Recent 429s increase cadence up to 1800s.

Auto defaults: threshold 90%, cooldown 300s, hysteresis 10 points, unhealthy
failover after 3 ticks. It compares all relevant windows and selects the
eligible account with most headroom, with additional reset-aware strategies
and per-account/weekly overrides. pace.py compares weekly consumption with
elapsed window fraction (15 points ahead is significant). aswap will
use conservative shared caching and max-window utilization with hysteresis.

`cswap run` uses a persistent isolated CLAUDE_CONFIG_DIR per account, seeded
with plaintext credentials. It shares selected settings via links by default;
history sharing is opt-in. `-- --resume` forwards resume to Claude, which
finds transcripts under projects/. Default auto swapping preserves the live
session. A profile-changing restart must explicitly preserve session history;
never invent a session ID or replay a prompt automatically on arbitrary errors.

macOS uses Keychain services (hashed config-dir service names for profiles)
with plaintext fallback. Windows uses file storage and copy-based sharing.
aswap initially supports Unix/Linux files only; it must not claim macOS
Keychain support. No keychain inspection is needed for development.

## Implementation boundaries

Store data only below XDG_DATA_HOME/aswap, with validated labels instead
of email-based paths, private directories and files, a process-wide store
lock and per-profile run locks. Metadata and secret bytes are separate;
metadata/usage output never serializes credentials. Error text must not
include provider response bodies, credentials, arbitrary filesystem paths or
child stderr. Tests create synthetic credentials at runtime in temp dirs.
Development commands never inspect real Claude/Codex homes.

Switch selects the default profile for aswap launchers; Claude additionally
supports an explicit default-file activation operation. Isolated run never
rewrites another profile. Avoid sharing writable auth files or replacing
Home Manager managed files. Running provider processes own refresh-token rotation;
only the explicit owner refreshes inactive profiles. Never restore stale backup credentials.

Codex uses CODEX_HOME profiles with auth.json. Quota should be read through
Codex's app-server account/rateLimits/read RPC, with initialize/initialized
handshake, rather than scraping a terminal UI or guessing a private endpoint.
Primary/secondary windows expose usedPercent, windowDurationMins, resetsAt;
missing limits (including API-key accounts) remain unknown. Multiple named
rate-limit buckets may exist. See official protocol documentation:
https://learn.chatgpt.com/docs/app-server and configuration documentation:
https://developers.openai.com/codex/config-basic .
No direct OpenAI API key is needed: aswap delegates auth to Codex.

## Implemented session behavior

`aswap switch` selects the profile for future `aswap run` invocations.
`--activate-live` explicitly requests the reference-style default credential
swap; symlinks and malformed global config are refused. This avoids silently
rewriting a Home Manager managed login. `auto ... watch --activate-live` is
the equivalent default-login monitor. Unrelated default processes are never
killed by the monitor.

`run` uses a private authentication view for both plain and supervised launches.
State is shared with the caller's provider home. `run --auto` forwards provider
arguments on the first launch, then preserves options while replacing selectors
with the exact tracked session ID and omitting one-shot prompt/print/stdin input.
Clean exits do not restart; forced switches send the configured follow-up unless
disabled. Restarts are capped at eight. No code changes the caller's cwd.

OAuth refresh now follows the provider-native flows below on an explicit owner.
Running provider CLIs retain refresh ownership through profile/identity leases;
inactive profiles can be refreshed before usage polling. Followers have no
refresh-capable credentials and consume the owner's access tokens and quota.

Interactive auto sessions use a Linux PTY, forwarding terminal I/O and window
size while preserving native provider UIs. Signal cleanup restores terminal
mode on normal handled termination. Before default-file activation, private
live-backups preserve the outgoing login; matching stable identity or exact
authentication associates provider-rotated auth with its stored profile.

## Profile quota display and identity

Usage transport and cache remain isolated per provider/label: Claude reads the
profile credential file, Codex receives that profile as CODEX_HOME, and each
profile has its own usage-cache.json. OAuth usage utilization and Codex
usedPercent are percentage points (0–100); values below one are not rescaled.
The old single remaining-quota line used the maximum utilization of all windows,
so equal weekly observations could conceal different session headroom. Status
now refreshes through the existing TTL/backoff gate and renders separate windows;
list stays offline and explicitly labels cached observation age. Automatic
selection and the JSON compatibility summary retain the conservative maximum.
Tests reproduce equal weekly limits with different session limits synthetically.

Human terminal views read email and tier hints from each profile's local config
and auth claims. Neither those hints nor account fingerprints enter JSON,
Waybar, cache files, diagnostics or errors. Duplicate matching compares SHA-256
hashes of stable account identity (including Claude organization when present),
with email as a fallback. It never hashes or compares raw tokens. Missing identity
is unknown, not a duplicate; identity extraction failures do not expose content.
Status preserves old observations during backoff but labels them unavailable;
expired windows are shown as last observed rather than current quota. Terminal
rendering strips control characters from local hints and wraps to terminal width.

## Label rename and forced-switch follow-up

Rename validates both labels and holds the store lock plus the old/new profile
leases. It moves the profile (including usage-cache.json and native histories)
and sessions/<provider>/<label>, then relabels the account and active selection.
Auto policy is provider-wide and survives unchanged, including cooldown and the
resume prompt. No separate label-keyed usage cache or policy exists. A private
rename-transaction.json journal makes interrupted moves recoverable: the next
store lock finishes the directory moves and atomic metadata replacement before
returning to a reader. Preflight refuses existing destination metadata/directories,
symlinks and running profiles. Lock files are retained to avoid inode-lock races.

New launch views live below runtimes/<provider>/<initial-label>/run-*; the
history owner's lease remains held across account switches to protect migration
and rename. Canonical transcripts live in the caller's shared provider home.
Legacy sessions/<provider>/<label>/run-* and anonymous sessions/run-* histories
are migrated as new files when the owning/all provider profiles are idle.

Auto settings persist resume_prompt as nullable text. Missing fields receive the
default follow-up; null disables it. A forced switch uses --resume <tracked UUID>
for Claude or resume <tracked UUID> for Codex. Prompts are literal argv elements,
never shell text. One-shot input is dropped with a generic note; options retain
their original spelling, order and values. Option arity is a parsing aid, not an
allowlist. CLI help can describe otherwise unknown options; if arity is ambiguous,
the initial launch still passes everything through but restart fails safely.

## Shared-state layout and migration

Use unprivileged per-entry symlinks, rather than user-namespace mounts: the
providers support config-home overrides and ordinary filesystem paths, symlinks
work without kernel/userns dependencies, and the layout is portable across the
three Linux machines. Auth is always a regular private file in the launch view;
canonical profile files retain only auth, account-bound config and aswap metadata.
All other ordinary home entries, including future provider entries, are shared.
Known history directories exist before symlinking so native directory creation
and transcript writers work. Settings/CLAUDE.md created during a launch publish
on exit; providers that replace a settings symlink are reconciled only if the
shared source has not changed concurrently. Managed root symlinks (files or
directories) are copied into a private view and never merged back. Migration and
reconciliation skip destination symlinks at every path component. Managed global
Claude config is read but never modified or replaced.

Claude's [official settings docs](https://code.claude.com/docs/en/settings)
identify account details, MCP config and project trust in ~/.claude.json; the
[storage documentation](https://code.claude.com/docs/en/claude-directory) locates
projects, instructions and history. Inspection of Anthropic's published npm
package @anthropic-ai/claude-code 2.1.81 (source, not any user's files) confirms
that oauthAccount, primaryApiKey and customApiKeyResponses participate in login;
userID/anonymousId identify the installation, and subscription/extra-usage and
feature-gate caches depend on the login. These fields stay private, including
userID as requested; hasCompletedOnboarding is also private to avoid repeated
login setup. Account/auth/oauth/organization/subscription-prefixed future keys
stay private conservatively. All remaining keys, notably projects (trust,
allowed tools, project MCP enablement), mcpServers, preferences and UI state,
are shared. Account-key classification is an explicit compatibility boundary,
not a claim of a documented exhaustive provider schema.

At launch, account keys are removed from the shared config and overlaid from the
selected profile. At exit and before a switch, only that profile's keys/auth are
saved back. Non-account changes use a recursive three-way merge against the
launch baseline under the store lock: unchanged keys do not write back, nested
project changes compose, and concurrent leaf changes win. The real home's
account keys never change. Native clients do not honor aswap's store lock, so a
native write racing the final atomic config replacement remains a provider-level
limitation; ordinary concurrent aswap runs merge without lost unrelated keys.

Migration streams only new files into shared state via no-clobber atomic publish.
Existing transcripts, indexes and symlinks are untouched. Originals and collisions
move to private migration-backups, including a full pre-split global config.
Profiles in use are skipped. Legacy auto directories with unknown owners are
migrated only while all provider profile leases are held. Legacy symlinks are
left in place rather than followed. No migration runs against real stores during
development: all tests create synthetic homes and transcripts outside Git.

## Exact session identity and cwd

[Claude's CLI reference](https://code.claude.com/docs/en/cli-reference) documents
--resume/-r, --continue/-c, --session-id and --fork-session. A new auto Claude run
preallocates a UUID through --session-id; explicit resumes retain their UUID.
Claude's published CLI registers sessions/<pid>.json with pid/sessionId/cwd.
The tracker also inspects /proc descriptors in the child's own process tree,
recognizing Claude <UUID>.jsonl and Codex rollout-...-<UUID>.jsonl filenames.
Only metadata and that bounded PID registry are read, never transcript contents.
If a picker/interactive switch or concurrent writer makes the identity ambiguous,
restart is refused; --continue/--last are never used as a fallback. Tracker
metadata observations are conservative; provider versions that move recording
outside the child process tree may require an explicit session UUID.

Commands inherit the OS working directory; no current_dir/chdir operation is
applied. Synthetic process tests invoke aswap from a separate project directory,
verify os.getcwd() inside both providers and through a forced switch, and assert
that the same transcript is visible after account changes.

## Usage errors, resets and refresh

Caches persist safe failure kind, HTTP status, Retry-After seconds, last attempt
and next poll. Diagnostic serialization uses a fixed class allowlist; arbitrary
cache strings, RPC errors, HTTP bodies and child stderr are never printed. Codex
RPC structured status fields or explicit HTTP status markers become numeric status
classes; otherwise start failure, timeout, unsupported method or RPC-unavailable
are explicit, with no invented HTTP status. A normal failure waits 60, 120, 240,
480 then at most 900 seconds. A longer Retry-After (seconds or HTTP date) wins.
Legacy unexplained multi-hour backoff is capped from its last recorded attempt.

A window with reset <= now whose reset has not yet been followed by a network
attempt bypasses TTL/backoff once. The attempt timestamp is recorded even on
failure, preventing a permanent passed reset from causing a poll storm.
`usage --refresh` explicitly bypasses that gate; missing provider means all
profiles. Stale data remains last-known utilization, dimmed and tagged stale,
never a question-mark bar or assumed empty quota. A provider token rotation
invalidates authentication backoff, while a quota endpoint Retry-After survives.

The refresh-owner role must be explicit; defaults never let three newly upgraded
monitors all begin rotating copied refresh tokens. Claude's published official
npm client @anthropic-ai/claude-code 2.1.81 defines JSON POST
https://platform.claude.com/v1/oauth/token with grant_type=refresh_token, public
client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e, the saved refresh token and saved
scope list (official default scopes when absent). It computes expiresAt in epoch
milliseconds from expires_in and retains the old refresh token only if omitted
from the response. aswap preserves unrelated credential fields and saved tier
hints. No custom endpoint/client-id environment override is accepted for refresh.
The endpoint is a provider interface, not a promised stable public API.

OpenAI's [native auth manager](https://github.com/openai/codex/blob/main/codex-rs/login/src/auth/manager.rs)
and [OAuth client](https://github.com/openai/codex/blob/main/codex-rs/login/src/oauth/client.rs)
use JSON POST https://auth.openai.com/oauth/token with public client ID
app_EMoamEEZ73f0CkXaXp7hrann. Rotated access/refresh/id tokens update the private
auth record and last_refresh, preserving omitted rotation fields and account ID.
JWT exp is an expiry hint, not proof of token validity. Failed/rejected or
malformed responses do not overwrite credentials; invalid_grant requires re-login
on the owner. There is no expiry-timestamp contest between machines.

Refresh happens under the store lock plus the nonblocking account lease, with a
fresh credential read after locking. Duplicate known identities share a hashed
identity lease as well as label leases, so a running alias excludes background
refresh. Successful rotations propagate to idle duplicate auth records while
preserving other extensions. Running views sync their CLI's fresh credentials
back at 30s cadence and exit. Usage-only Codex app-servers use a private temporary
access-only auth snapshot, so they cannot rotate behind a running CLI. No real
credential or transcript files were inspected or used in development tests.

## Multi-machine design: one authority, access-only followers

The simplest robust implementation is an explicit single refresh/usage owner,
not bidirectional newest-file or newest-expiry wins. That heuristic cannot prove
that a rotating refresh token is still valid. `sync --owner` enables local
background refresh; `sync SSH_ALIAS` configures a follower and pulls the owner's
profiles and persisted usage cache. The owner is reached through SSH batch mode,
no PTY, normal known-host checks, a fixed remote command and bounded private
stdin/stdout protocol. No credential is placed in argv, stderr or normal CLI JSON.
The hidden server refuses terminals and non-SSH invocations. Auth transfer is
necessarily credential-bearing inside the encrypted SSH pipe; it is never human
output, a log, or a public-repository artifact.

The owner exports no OAuth refresh token, including nested credential extensions.
Peers cannot invalidate its refresh chain. Local canonical auth is access-only;
launch views adapt to native token reload behavior: Claude gets a public
non-credential refresh sentinel plus a delayed local refresh deadline. Its
published 401 recovery first requires a nonempty refresh field, re-reads the auth
file, and retries when the access token has changed. The sentinel enables that
reload path but can never authorize a real rotation; proactive sentinel refresh
is suppressed. Codex launch views use the external chatgptAuthTokens mode and an
empty refresh token. Peers install the owner's fresh access token into live views
at 30s cadence. If the owner is unreachable, a stale sentinel may yield an auth
error rather than any rotation; no peer silently becomes an owner. Provider token
reload behavior is version-sensitive and synthetic process tests cannot prove
arbitrary future provider versions; explicit resume is the recovery path.

Every owner request shares its existing store-locked per-profile TTL/backoff, so
three monitors do not triple quota API traffic. Peers serialize SSH pulls and
schedule them using receipt time on their own clock, not remote wall-clock order.
Clock skew is never evidence that one credential is valid. Quota resets and JWT
expiry still require normally synchronized system clocks. Unmatched local
profiles keep their private files, get an owner-profile-missing quota state and
cannot become eligible through stale cached headroom. Known label/identity
collisions are refused. Sync excludes histories, shared settings and local auto
policy. Profile administration/login is owner-side and coordinated while affected
profiles are idle; local account data is never deleted by sync.

Recommended deployment: stop all monitors and existing runs for ownership bootstrap;
choose the always-on server as owner;
re-login there if prior independent rotations invalidated its tokens; assign
owner/follower roles; pull from laptop and desktop; then start their monitors.
Use aswap launch views on peers rather than independent native refresh-capable
copies. The SSH command must find the same aswap binary/store environment as the
owner service. OAuth rotation by software outside this protocol, provider
revocation, a crash after remote rotation but before local persistence, and a
network partition cannot be solved by timestamp merging: report the safe failure
and reauthenticate on the owner. Never push an unverified copied refresh token
back from a follower. Entering or leaving follower mode requires idle profile
leases, preventing a running process from retaining an old refresh-capable view.

Home Manager refreshOwner is nullable (null preserves CLI role, true owner,
false with no syncHost disables background refresh); syncHost is an SSH alias.
Service setup applies role before threshold/prompt/on. Tests cover cache reset
fetching, HTTP/error redaction, backoff/Retry-After, forced refresh, native OAuth
request/rotation semantics, lease exclusion and aliases, managed-file preservation,
owner cache reuse, access-only packets, SSH failures and clock skew.
