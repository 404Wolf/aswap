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
Home Manager managed files. Provider processes own refresh-token rotation;
read the current profile for usage, never restore stale backup credentials.

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

`run --auto` is a separate interactive supervisor. It keeps a stable private
session directory, copies back provider-rotated credentials to the correct
account, and changes only the runtime account when quota reaches the line or
terminal output reports a provider limit. Restart uses `claude --continue` or
`codex resume --last`, retaining that run's history. It sends SIGTERM first,
allows five seconds to exit and falls back to SIGKILL. After a forced switch,
a configurable follow-up prompt resumes work automatically; a clean exit never
sends it. Pending user commands are not replayed. Provider arguments are refused in this mode and restarts are capped
at eight. Auto session histories remain private under the data store's
sessions directory. Plain `run -- --resume` and `run -- resume --last` use the
account's persistent profile history. Isolated profiles do not import the
user's settings, plugins, project transcripts or MCP servers automatically.

Token refresh is delegated to the official CLI. Expired inactive Claude
credentials can report unknown usage until that profile is logged in or
launched again; aswap does not implement a second OAuth refresh client.

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

New auto histories live below sessions/<provider>/<initial-label>/run-*; the
history owner's lease remains held throughout account switches. Anonymous legacy
sessions/run-* histories contain no label reference or ownership manifest and
remain intact at their original paths; rename does not guess their ownership.

Auto settings persist resume_prompt as nullable text. Missing fields in old stores
receive the default follow-up; null disables it. On a forced limit/threshold
switch, the supervisor launches Claude with --continue -- <prompt> or Codex with
resume --last -- <prompt>. Each prompt is one literal argv element, never shell
text. The installed CLIs' help confirms positional prompt support; Codex's
[session-command parser](https://github.com/openai/codex/blob/main/codex-rs/cli/src/main.rs)
also explicitly supports --last PROMPT. With the setting off, only native resume
arguments are sent. Success without a forced-switch target exits immediately,
even if terminal output happened to include a limit-like string. Arbitrary errors
still do not trigger resume or prompt replay.
