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
