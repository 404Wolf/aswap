# aswap

aswap keeps several Claude Code and Codex CLI logins on one Linux machine and
picks which one each session uses. It can switch a running session to another
account when the current one hits its usage limit, resuming the same
conversation. It also shows quota per account in the terminal and in Waybar.

## Where the source lives

The source lives on Wolf's private Gitea, as `instant-fill/aswap`. Push only
there. `github.com/404Wolf/aswap` is a read-only mirror: Gitea pushes to it
automatically on every commit, so don't push to GitHub directly.

## Setup

### Home Manager

Add the flake as an input and import its module:

```nix
{
  inputs.aswap.url = "git+https://github.com/404Wolf/aswap";

  # In your Home Manager configuration:
  imports = [ inputs.aswap.homeManagerModules.default ];
  programs.aswap = {
    enable = true;
    autoswitch.claude = { enable = true; threshold = 90; };
    autoswitch.codex.enable = true;
    waybar.enable = true;           # adds custom/aswap under mainBar
  };
}
```

### Nix, without Home Manager

```sh
nix profile add git+https://github.com/404Wolf/aswap
# or, from a checkout:
nix build        # result/bin/aswap
```

The official `claude` and/or `codex` CLI must be on `PATH`. aswap does not
install them.

### Log in and add accounts

Log in with the provider's own CLI, then save that login under a label. Repeat
for each account:

```sh
claude auth login        # or: codex login
aswap add claude work
aswap add codex personal
aswap list               # saved accounts
aswap status             # accounts with quota
```

### Switch and run

```sh
aswap switch claude work                # account for future runs
aswap run claude                        # run with the selected account
aswap run claude --account work         # or name one
aswap run claude --auto -- --resume     # switch accounts at the usage limit
aswap auto claude threshold 90          # when --auto switches
```

Arguments after `--` go to the provider unchanged.

When a shared limit or reset hits many `--auto` sessions at once, they switch
one at a time: across all providers and sessions using the same aswap data
directory, at most one switch starts every `switch_interval_seconds` (default
4), first come first served. A delayed session prints its place in line. Set it with
`aswap auto switch-interval 4` (0 turns it off), or the Home Manager option
`programs.aswap.switchIntervalSeconds`. `aswap list` and `aswap status` show
it. First launches are not delayed.

Use `aswap auto claude reserve work` to keep that account out of automatic
selection. A run started with `aswap run claude --account work --auto` can
use it and fall back to other accounts when it runs out of quota. Reservations
appear in `aswap list` and `aswap status`. Use `aswap auto claude unreserve work`
to return it to the automatic pool.

### Use the quota that expires first

By default (`weekly-reset`), when several accounts have room, aswap uses the
one whose weekly window resets soonest, so quota that would expire unused goes
first. New launches and the monitor also move to such an account. This is the
default for Claude and Codex whenever the state sets no preference; a
preference you set explicitly is kept. To rank by the most headroom instead:

```sh
aswap auto codex prefer headroom        # or: aswap auto claude prefer headroom
```

The 5-hour session window does not affect this ranking: it renews every 5
hours whatever you do.
Past the switch threshold, aswap's last-resort fallback ranks by the most
headroom instead, to keep the session running as long as possible.

## Codex usage-limit resets (opt-in)

Codex accounts can hold a few **usage-limit reset credits** (in Codex, `/usage`
→ Redeem reset). aswap can redeem one for you when every Codex account is out
of weekly usage, so supervised sessions don't sit blocked for days.

Read this before you enable it:

- **A redemption resets the account's eligible windows** (Codex's contract
  says "eligible", not which ones) and **spends one earned credit**. Credits
  are limited and expire; a spent credit can't be returned.
- **When.** aswap redeems only when every *usable* Codex account has no
  usable quota now: its weekly window used up and not yet reset. (A backend
  denial, `ordinaryUsageAllowed: false`, with weekly room left is a hold, not
  a spend, unless the session-window opt-in below is on.) Reserved and
  signed-out accounts are not usable, so they neither block a reset nor get
  one. Unknown usage on a usable account means no redemption; aswap reads it
  first. *(This reading of "exhausted" is Wolf's call; see the PR.)*
- **Weekly only, by default.** A used-up 5-hour window alone never spends a
  reset: every usable account must be out of weekly usage. To also redeem
  when accounts are only 5-hour-limited, opt in with
  `aswap auto codex reset-session-windows on` (Home Manager
  `programs.aswap.autoswitch.codex.resetSessionWindows = true`).
- **Which account.** The usable account whose quota would come back by itself
  the latest, i.e. the latest reset among its used-up windows. For accounts
  out of weekly usage that is the weekly reset, so 7 days outranks 3 days.
  Ties go to the soonest-expiring credit, then to the label.
- **Unknown is not "none".** If an account's credits aren't known (only a
  count, or no count) and it could be the right one, aswap waits rather than
  spend on another.
- **Once only.**
  - Each redemption gets an idempotency key, journaled (fsynced,
    append-only `codex-resets.journal`) before anything is sent.
  - Retries and a restart after a crash resend the same key, so Codex
    answers `alreadyRedeemed` instead of spending again.
  - At most one credit per decision, then an hour's cooldown.
  - Mode changes wait for a running decision, and the mode is rechecked
    right before sending.
- **Never forgotten.** A request that got no answer (three tries, a mode
  change, quota coming back) may have spent its credit. It is paused, with
  its key, never replaced by a new one. `aswap auto codex reset retry`
  settles it: if the account reads usable again, the request evidently
  worked and is recorded as redeemed, with nothing sent; if it is still out
  of usage and the pool still qualifies (the same checks as a new spend),
  the same key is resent; otherwise it stays paused. `aswap auto codex reset
  dismiss` drops it.
- **Resuming.** Once a reset is redeemed and a fresh read shows the account
  usable, aswap journals that (`recovered`) and waiting `run --auto` sessions
  check at once (not at their next scheduled check); a session held on that
  very account is released. A failed or still-full post-reset read is
  retried on a short backoff, never with a new spend. Where Codex reports `ordinaryUsageAllowed`, only
  a grant counts as recovery.
- **Money.** It uses only credits Codex lists as available. If a credit's
  title or description, or Codex's answer, mentions payment, purchase,
  billing, upgrades, credits or extra usage, aswap refuses and stops
  redeeming until you run `aswap auto codex reset off`. The halt is
  journaled first, so it survives a crash.
- **Who.** Only the monitor (`aswap auto codex watch`, the `aswap-codex`
  service) on the refresh owner redeems; sync followers never do. The
  guarantees hold within that one data directory; nothing coordinates across
  machines beyond the single refresh owner. With Home Manager, `autoReset`
  needs `autoswitch.codex.enable = true`.

Try the dry run first. It logs `would reset codex:LABEL (weekly resets in …)`
to the monitor's output and to the reset log, and spends nothing:

```sh
aswap auto codex reset dry-run   # log what it would redeem
aswap list --json | jq .codex_usage_resets
aswap auto codex reset on        # redeem for real
aswap auto codex reset off       # the default; also clears a halt
aswap auto codex reset retry     # resend a paused request (same key)
aswap auto codex reset dismiss   # drop a paused request
```

With the Home Manager module, set
`programs.aswap.autoswitch.codex.autoReset = "dry-run";` (or `"on"`; default
`"off"`).

aswap redeems with the same `account/rateLimitResetCredit/consume` request the
Codex TUI sends through `codex app-server`, using the account's own login. It
records each attempt, with the account's usage before and after, in
`codex-resets.json` in the aswap data directory (the last 50 entries), and
every intent, attempt and outcome in the append-only `codex-resets.journal`,
which is the authority after a crash.

`aswap sessions --json` shows, for each running session, whether its account
is held by a usage limit (`limit_blocked`) and, when Codex or Claude said,
when it frees up (`blocked_until`, Unix seconds).

## Reading usage from another program

`aswap list --json` is the stable, cache-only interface: it makes no network
request and is safe to poll (the Herdr Usage view polls it every minute;
over hc: `hc exec --on <machine> aswap list --json`). Fields are only ever
added under `schema_version` 1. For each account:

- `usage.windows[]`: `name`, `used_percent` (0–100), `resets_at` and
  `duration_minutes`. Times are Unix seconds.
- `remaining_percent`: 100 minus the highest window, or null when the
  reading isn't usable.
- `usage_status`: `ok`, `backoff` (see `usage_error`) or `unknown`.
- `usage_reason`: why: `fresh`, `never-fetched`, `error`, `unverified-limit`,
  `stale` (older than 10 minutes), `reset-passed` or `no-windows`.
- `age_seconds`: seconds since the reading, by this machine's clock; null if
  never fetched.

`aswap usage --json` returns the same shape after a read that respects backoff.
The JSON never carries tokens, emails or paths; the plain-text output shows
identity hints, so don't parse it.

## How it works

- Each account's login is stored privately under `~/.local/share/aswap/`
  (0700 directory, 0600 files). Nothing is stored in the repository.
- A run gets its own provider home with that account's credentials. Everything
  else (conversations, settings, MCP servers, skills) is symlinked to your
  normal `~/.claude` or `~/.codex`, so every account sees the same history.
- aswap reads each account's quota from the provider (Claude's OAuth usage
  endpoint, Codex's app-server) and caches it.
- With `--auto`, aswap runs the provider in a PTY. When it sees a usage limit
  or an expired login, it stops the provider and resumes the same session on
  the account with the most quota left.
- With several machines, one machine (`aswap sync --owner`) refreshes the
  OAuth tokens and pushes them to the others, so they never refresh the same
  token at once.

[DESIGN.md](DESIGN.md) has the details.

## Runtime retention

Runtime retention is off by default. Enable it in
`$XDG_CONFIG_HOME/aswap/config.toml` (usually `~/.config/aswap/config.toml`):

```toml
[retention]
enabled = true
min_age = 172800          # seconds, two days
dry_run = true
io_pressure_limit = 30    # Linux /proc/pressure/io some avg10, in percent
# archive_dir = "/absolute/path/to/private/archive"
```

With the Home Manager module, set `programs.aswap.retention.enable = true`.
Its `minAge`, `dryRun`, `ioPressureLimit`, `archiveDir`, and `legacyHomes` options configure
the same settings. The default archive is `archive/` inside the aswap data
directory (`~/.local/share/aswap/` by default).

```sh
aswap gc --dry-run
aswap gc --apply
aswap gc --dry-run --json
```

The report goes to standard output with a verdict, reason, and byte counts
for each view, followed by totals. Preview estimates count logical file bytes
and exclude bytes to move or archive. Apply counts net logical bytes after
files moved to the shared home and newly written compressed archives. An apply
that stops early or leaves an operation incomplete exits with a nonzero status.
Actual filesystem space varies with compression, sparse files, and hard links.

GC processes one view at a time at idle I/O priority and nice 19. It stops if
I/O pressure exceeds the configured limit or the pressure reading is unavailable.
It keeps young views, views with a held lock, and views referenced by a live
aswap supervisor or a local process under the store's Unix user. The process
check uses open file descriptors, provider home variables, and working directories.
GC takes a fresh descriptor scan after hiding each view's original name.
If Linux denies a read, GC checks the process command line. It keeps the
view if the command line is unavailable, empty, or mentions the view, Claude,
Codex, aswap, or Node. The report counts unreadable non-provider processes it
skipped, once per PID. GC rechecks ambiguous descriptor snapshots against the
current process state. It also retries failed working-directory and environment
reads. When a process still makes liveness uncertain, the reason includes its PID
and a redacted command-line excerpt. Views containing mount points stay in place.

GC compares the size and SHA-256 of duplicate transcripts. Unique transcripts
move to the same relative path in the shared provider home. GC keeps unmarked
legacy views until you confirm the original home for that label. Supply the
same mapping in preview and apply, using repeatable `--legacy-home` flags or
the `[retention.legacy_homes]` TOML table. Only map a label whose original home
you have verified.

```sh
aswap gc --dry-run --legacy-home 'codex:old-label=/absolute/path/to/codex'
```

Codex rollouts remain under `sessions/YYYY/MM/DD/rollout-*.jsonl` or
`archived_sessions/` and can be opened with `codex resume <id>`. Claude
transcripts remain at `projects/<slug>/<id>.jsonl` and can be opened with
`claude --resume <id>` from the matching project. If both paths had different
data, GC leaves the shared file in place. Conflicting transcripts and any
view-local private files without a verified identical shared copy go into an
owner-only archive at `archive/<provider>/<label>/<run>/`. GC always snapshots
private SQLite databases, including committed WAL data, even when the main file
matches the shared copy. The snapshot checks I/O pressure between batches of
pages and times out after a sustained lack of progress. GC prunes
`logs_*.sqlite` groups as regenerable. GC unlinks all-symlink SQLite groups without opening
their targets. It also discards regenerable SQLite shared-memory files and
empty regular `*.lock` files. A linked or unrecognized sidecar that could
affect private SQLite state keeps the view. The report names the sidecar
relative to the view. With the main file missing, GC preserves the raw WAL or
journal under `orphan-sidecars/<sha256>/` in the view's archive. The checksum
manifest covers those payloads too. If a concurrent restore changes the
archive displaced during publication, GC keeps both generations and the view.
GC publishes the verified archive before retiring the source. If a later check
keeps the view, the archive remains available for retry and the report lists
its path. GC checks every sealed payload's identity and hash again before removal.
From the archive, run `sha256sum --check SHA256SUMS` to check the compressed
files before restoring the one you want.

When Codex's shared thread index points into a view, GC verifies the shared
rollout and updates the index before removing the view. An ambiguous index
reference keeps the view. Apply reports the verified SQLite backup path when
it first rewrites a shared index. Before deletion, GC renames the view and takes
a Codex index write transaction to check for references to both names. A native
Codex process holding an old path in memory can still republish it after that
transaction. Replacing the shared Codex home directory while GC is updating
the index can also race with opening that database. Apply restores an
interrupted retirement to its original path when it can do so safely. If I/O
pressure interrupts SQLite cleanup after the WAL has been removed, a retry can
keep the partial view because its remaining main file differs from the archived
snapshot. The verified archive retains the committed data. Reclaim counts
include newly written index backups.

## Development

```sh
nix develop                          # also installs the secret-scanning git hooks
scripts/isolated cargo test --locked
nix flake check
```

Tests use synthetic fixtures only. Never point them at real logins.

Using several accounts to avoid usage limits may break the providers' terms.
aswap is not affiliated with Anthropic or OpenAI.
