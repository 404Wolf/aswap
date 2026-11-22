# Changelog

## Unreleased

### Fixed
- **A newer owner no longer breaks an older follower's sync.** A sync packet with a profile of a provider the follower doesn't know now skips that profile instead of failing the whole packet. And settings only a newer aswap knows stay out of `--json` output.
- **A rotated login survives a failed write.** When a token refresh succeeded but writing the new login failed (a full disk, EIO), the provider had already invalidated the old refresh token, and the new one was only in memory: the account needed a new login. The new login is now written first to two private pending copies: one beside the login, and one under `$XDG_RUNTIME_DIR` (a tmpfs, so a full or failing data disk keeps one). Each records which login file it replaces. The next refresh installs a copy only over that same file, so a stale copy never rolls back a newer login. `add` and a preserved live login drop any copies. (Audit asw-0ti P2-5.)
- **state.json written by a newer aswap is kept whole.** Unknown top-level fields, unknown per-provider settings, and the accounts, active labels and settings of providers this version doesn't know were dropped on save, or (an unknown provider) made state.json unreadable. A long-running older supervisor could then undo a newer one's `reserved` accounts or switch interval, and a future provider's account would stop every older aswap. They are now read past and written back unchanged. A known provider's malformed settings are still an error. (Audit asw-0ti P2-3.)
- **Codex usage failures no longer rotate the login.** A failed Codex usage read forced a token refresh for nearly any failure: a changed response shape, an app-server that exited early, too many notifications. Each refresh revoked the access token of every running Codex session and restarted them, on every poll. Now only an explicit 401, or an app-server failure while the stored access token has expired, forces one. (Audit asw-0ti P2-2.)
- **A slow usage poll no longer freezes every session's typing.** Usage fetches (a Claude HTTP call or a Codex app-server request, up to 10–20 s) ran under the store lock that every supervisor takes on its terminal thread. They now run outside it, one at a time per account under a per-account usage lock. A second reader returns the cache at once; an explicit `--refresh` waits up to 20 s for it. A supervisor's 30-second refresh check also no longer takes the store lock unless the login is close to expiry. (Audit asw-0ti P2-1.)
- **A Codex session no longer fails over on someone else's 429.** On a Codex terminal, Claude's limit wording also matched: `error: ... 429`, `rate limit reached`, `you've hit your weekly limit`, and Claude's compaction and limit dialog. So tool output Codex draws, such as `└ Error: 429 Too Many Requests` from curl, switched a healthy account away. Codex terminals now count only Codex's own notices (`■ You've hit your usage limit ...` and the bare `usage_limit_reached` / `rate_limit_exceeded`). Claude terminals are unchanged. A Codex notice in the first 15 s after a launch or resume counts only when the account's quota reads exhausted (the cached reading, or one fresh read if that predates the launch), when the login rotated since launch (aswap then reloads the same account with it), or when Codex then exits with a failure. A redrawn old notice on resume no longer switches accounts. (Audit asw-0ti P2-4.)

## 0.2.1 (2026-11-21)

### Added
- **Why a reading isn't usable.** Each account in `list`/`usage`/`status` JSON now carries `usage_reason` (`fresh`, `never-fetched`, `error`, `unverified-limit`, `stale`, `reset-passed` or `no-windows`) and `age_seconds` (seconds since the reading, by the machine's clock). `remaining_percent` is set exactly when the reason is `fresh`.
- **`list --json` is a pinned interface.** A test fixes its keys and the `usage_status` values, and the README documents it as the stable, cache-only way for other programs (the Herdr Usage view) to read usage. Fields are only ever added under `schema_version` 1.

## 0.2.0 (2026-11-21)

### Added
- **Opt-in Codex usage-limit resets** (`aswap auto codex reset off|dry-run|on`; Home Manager `programs.aswap.autoswitch.codex.autoReset`). Off by default.
  - When every Codex account is out of weekly usage, the Codex monitor on the refresh owner redeems one earned usage-limit reset credit. It picks the account whose weekly window resets farthest away.
  - A redemption resets that account's 5-hour and weekly windows together.
  - Credits are limited and expire, and a spent credit can't be returned.
  - `dry-run` only logs what it would redeem and spends nothing.
  - Redemptions are limited to one per decision, then an hour's cooldown. Retries reuse their idempotency key, and anything that mentions payment is refused.
  - `aswap list --json` shows the reset log under `codex_usage_resets`.
- **Machine-wide switch gate.** Across every provider and every `run --auto` session sharing the aswap data directory, at most one account switch starts every `switch_interval_seconds`, first come first served. The default is 4 s; 0 turns it off.
  - This stops a shared limit or reset from relaunching every session at once.
  - Set it with `aswap auto switch-interval N` or Home Manager `programs.aswap.switchIntervalSeconds`; `aswap list` and `aswap status` show it.
  - Lock waits and writes are bounded, a busy lock is waited out for up to 30 s, and a gate that can't be used lets the switch through with a warning.
- **macOS build.** aswap now compiles on macOS, and the switch gate's tests pass on `macos-latest`. It doesn't run there yet; the gaps are listed below.

### Changed
- **Soonest weekly reset** (`prefer weekly-reset`) is now the default preference for both Claude and Codex. Quota that would expire unused goes first. Set `prefer headroom` for the old behaviour.

### Fixed
- Test fixture races: quota revocation, the sync stream, and the refusing watch owner.
- A retried Codex reset that was refused for mentioning money is recorded as unconfirmed.

### Known gaps on macOS
- `aswap run` fails at startup: session leases read the boot ID and process start times from `/proc`.
- Process liveness and the follower environment check use `/proc/<pid>`.
- Session tracking (`aswap sessions`, and exact-session resume after a switch) reads `/proc/<pid>/fd` and the process's children.
- Runtime retention (`aswap gc`) needs `/proc` (processes, mounts, I/O pressure, fd paths) and Linux `openat2`; on macOS it refuses rather than misbehaves.
- Without Linux's parent-death signal, a SIGKILLed supervisor leaves its provider running.
- Claude Code keeps its macOS login in the Keychain, which aswap doesn't read.
- The Home Manager module defines systemd user services only (no launchd).

## 0.1.0

Initial version: account profiles for Claude Code and Codex, supervised `run --auto` switching, usage status, sync, Home Manager module and retention.
