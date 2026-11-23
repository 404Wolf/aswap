# Changelog

## 0.2.2-preview.agy.7 (2026-11-23)

agy.6 with its review's changes (t4hf; t2kg's rulings). Not a release.

### Fixed
- **The owner sends the provider's own answer for a follower's request,** labelled with that request's id, never a second, cached reading. A read whose cache commit fails confirms nothing.
- **A confirming read never replaces a newer reading,** and doesn't confirm anything when a newer one landed while it ran.
- **`switch --activate-live` never saves the outgoing live login over a profile's login.** Nothing can prove it newer (an old cached 401 isn't bound to the current login), so this fails closed. The live login stays in live-backups. The generation history from agy.5 and agy.6 is gone.

## 0.2.2-preview.agy.6 (2026-11-23)

agy.5 with its review's changes (t4hf; t2kg's rulings). Not a release.

### Fixed
- **No lock-order deadlock between a token refresh and an activation.** Everywhere, an account's refresh lock comes before the store lock. Holders of the store lock only try the refresh lock (`switch` and `rename` let go and retry). The refresh takes its locks first.
- **A follower takes only its owner's answer to its own request.** The follower sends a request id with its forced sync. The owner echoes it on that account only when it read the provider for that request, and the follower accepts nothing else. An older owner never echoes, so it confirms nothing.
- **Credentials are told apart by their token identity, never their bytes.** If the generation history can't be written, a rotation's install fails closed: the new login stays in its pending copies. Alias propagation records generations too. The outgoing live login is saved into its profile only when provably newer: a different credential, not one the profile replaced, while the profile's own login was rejected. Otherwise it is kept only in live-backups.
- **An answer with no usage in it** counts as no answer, never as room.
- **A confirming read takes no per-login fetch lock,** so an abandoned read can't block the next one.
- **The exit fallback** (a notice, then no read answering at all, at a failed exit) applies only when the notice is in the session's final screen, its last 12 rows of output. A quote further up, or an unrelated exit, counts nothing.

## 0.2.2-preview.agy.5 (2026-11-23)

agy.4 with its review's changes (t4hf; t2kg's rulings). Not a release.

### Fixed
- **A follower confirms a limit from its refresh owner.** It asks the owner for a forced read and accepts only an observation newer, on the owner's clock, than the one it held. An owner that can't be reached confirms nothing.
- **Which login is newer is decided by generation, not by expiry or file times.** A profile remembers the logins it replaced. An outgoing default that is one of them is older and never saved over the current one, even when the rotated token expires sooner. Any other default login is newer, such as a provider refresh in place or a new sign-in.
- **A pending login that can't be read is kept,** and the account isn't refreshed or activated until it can be.
- **Claude's screen-only notices** (no transcript known) go through the same fresh-read confirmation as Codex's. Claude's transcript decides as before.
- **The token request never holds the store lock,** so no supervisor's terminal waits on another's network call. The refresh and identity locks still exclude other refreshers.
- **A Ctrl-C typed during the exit check** ends it at once and counts nothing, like a signal.
- **A trigger whose latest read failed is kept** past the 5-minute window. Only a latest answer showing room retires it. A read hung for 60 s is replaced.
- **An account denied ordinary usage** (`ordinaryUsageAllowed: false`) reads as used up everywhere, so it's never a recovery target.

## 0.2.2-preview.agy.4 (2026-11-23)

agy.3 with the review's changes (t4hf; t2kg's rulings). Not a release.

### Fixed
- **A Codex notice counts only with a fresh confirming read, while running.**
  - Reads that fail or don't answer are retried and never make a live session switch.
  - A failed exit re-reads for the whole 30 s, every 5 s. Only if none of those reads answered does the notice count, after the full window. A signal during that wait stops it, and nothing counts.
  - A confirming read is the provider's own answer to a read started after the notice (`usage::fresh_read`), never a cached reading, even one from the same second.
  - `ordinaryUsageAllowed: false` counts as exhaustion.
  - A trigger is kept until an answer resolves it. One read runs at a time, kept until it answers.
  - A rotated login no longer counts as a limit. It only lets the supervisor reload the same account.
- **Activation never writes an older login over a newer one.** Under the refresh lock, the outgoing default is saved into its profile only when it is newer. Newer means a later expiry, or, without one, the file written last. A rotation that completed while the activation waited is kept. An incoming account's pending rotation is installed before it goes live.

## 0.2.2-preview.agy.3 (2026-11-23)

0.2.2-preview.agy.2 (#44 plus the wake fixture fix) with fixes for two bugs it inherited from main. Server-ops found both in their review of agy.2. Not a release.

### Fixed
- **A Codex session no longer switches accounts on text that looks like a limit notice.** A marked "■ You've hit your usage limit" row followed by any text counted as a limit, and 15 s after launch it was believed without a quota check. A healthy session that showed such text, a redraw or a model-specific bucket therefore failed over. A Codex notice is now only a trigger. It counts once a fresh quota read taken after it shows the account exhausted. The read runs off the terminal thread and is retried for up to 5 minutes, because a reading can lag the limit. The trigger is lenient: any marked row with the phrase, wrapped or split anywhere, with any promo copy, so no real limit is missed. Codex's wording no longer counts on a Claude terminal at all. After a failed exit the account is re-read for up to 30 s. A failed read then counts the limit, so a session is never left on an account that can't be checked.
- **`switch --activate-live` no longer loses a rotated login.** It deleted a pending rotated-login copy and wrote stale bytes over the outgoing profile. It now leaves both for normal rotation recovery. This is server-ops' option-B fix, reviewed as correct in all four of its review rounds.
- Tests: every fixture from option B's four independent reviews is replayed one byte at a time on Codex and Claude terminals. The confirmation gate covers a lagging read, a failed read, a stale cache and a failed exit. The pending-login test is option B's.

## 0.2.2-preview.agy.2 (2026-11-22)

The same product as 0.2.2-preview.agy.1 (#44 at 1c627b8). Only a test fixture changed: each fake provider in the wake tests writes its own temp file (from #45), so a shared-path race no longer fails them. Not a release.

## 0.2.2-preview.agy.1 (2026-11-22)

A preview of #44 (agy) for the VM, on main c0ff893. Not a release.

### Added
- **Antigravity CLI (agy) accounts.** `aswap add agy <label>` and `aswap run agy --auto` pool personal Google sign-ins of Google's agy. Each run gets its own `--gemini_dir`, config home and no D-Bus, so logins never share a keyring. Conversations, plugins, hooks and MCP servers are shared. agy refreshes its own login and aswap writes the newer one back; aswap never calls Google's token endpoint. A failover resumes the conversation by id, and quota notices are agy's own. Billed sign-ins and paid credits are refused. agy accounts are never synced, and adding one refuses while an older aswap build runs. See README, "Antigravity CLI (agy) accounts".

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
