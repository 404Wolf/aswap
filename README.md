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

Use `aswap auto claude reserve work` to keep that account out of automatic
selection. A run started with `aswap run claude --account work --auto` can
use it and fall back to other accounts when it runs out of quota. Reservations
appear in `aswap list` and `aswap status`. Use `aswap auto claude unreserve work`
to return it to the automatic pool.

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
skipped, once per PID. Views containing mount points stay in place.

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
matches the shared copy. A linked or unrecognized private SQLite sidecar keeps
the view. GC checks every sealed payload's identity and hash again before
removal. From the archive, run
`sha256sum --check SHA256SUMS` to check the compressed files before restoring
the one you want.

When Codex's shared thread index points into a view, GC verifies the shared
rollout and updates the index before removing the view. An ambiguous index
reference keeps the view. Apply reports the verified SQLite backup path when
it first rewrites a shared index. Before deletion, GC renames the view and takes
a Codex index write transaction to check for references to both names. A native
Codex process holding an old path in memory can still republish it after that
transaction. Replacing the shared Codex home directory while GC is updating
the index can also race with opening that database. Apply restores an
interrupted retirement to its original path
when it can do so safely. Reclaim counts include newly written index backups.

## Development

```sh
nix develop                          # also installs the secret-scanning git hooks
scripts/isolated cargo test --locked
nix flake check
```

Tests use synthetic fixtures only. Never point them at real logins.

Using several accounts to avoid usage limits may break the providers' terms.
aswap is not affiliated with Anthropic or OpenAI.
