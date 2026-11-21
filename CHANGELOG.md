# Changelog

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
