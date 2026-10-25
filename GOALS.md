# agentswap: one tool to juggle Claude Code and Codex accounts

Project naming note: the crate, binary, CLI, data directory and module namespace are `aswap`; the checkout path and headline stay unchanged.

A Rust port of claude-swap (github.com/realiti4/claude-swap, MIT, © 2026 Onur
Cetinkol; reference checkout: /tmp/claude-1000/-home-wolf-Config-home-manager/8b14e281-91e3-493a-b0fb-70012e093b84/scratchpad/claude-swap),
extended to OpenAI Codex CLI. Tick items (`- [x]`) as they land, with tests.
Never edit the headline (line 1).

## SAFETY RULES (non-negotiable; this repo is PUBLIC)

- Never commit or push credentials, tokens, account stores, auth.json,
  .credentials.json, real usage data, real email addresses, or anything from
  the user's home directory. Tests use synthetic fixtures only.
- Account data lives outside the repo: `$XDG_DATA_HOME/agentswap/` (0700 dir,
  0600 files). Never log secrets; redact tokens in errors and debug output.
- Secret scanning must gate every commit and push: a gitleaks (or equivalent)
  pre-commit and pre-push hook in the repo (installed via the dev shell), plus a
  gitleaks job in CI. Before ANY push, run the scanner over the full history
  and over `git diff origin/main..HEAD` (or the whole tree for the first
  push), and review `git status` and `git ls-files` by eye.
- Don't push until the user's operator (Claude, in the left pane) says so.
  Prepare commits locally; report when ready.

## M0: Understand claude-swap
- [x] Read the reference implementation. Write DESIGN.md: features, where it
  stores data, how it reads 5-hour and weekly usage (endpoints/headers),
  how switching works (credential file swap vs. CLAUDE_CONFIG_DIR), how
  sessions are resumed, the autoswitch thresholds and pacing, parallel sessions.
  Note anything platform-specific (macOS keychain vs. Linux files).

## M1: Core (Rust)
- [x] A provider-agnostic account store with an `Account { provider, label, ... }`
  model, atomic writes, file locking, and 0600 perms.
- [x] Claude provider at feature parity with claude-swap on Linux: add the
  current login, list, switch, remove, usage (5h/weekly) with caching and
  backoff, and autoswitch to the account with the most headroom at a
  threshold (default 90%), resuming the session where claude-swap does.
- [x] Codex provider: profiles via CODEX_HOME (auth.json per account), the same
  commands, and usage from what Codex exposes (rate-limit headers or its
  status/usage output; document what's available). Autoswitch on usage or
  on 429/limit errors.
- [x] Parallel sessions: run `agentswap run claude|codex [--account X] -- args`,
  which launches with that account's isolated config dir, so two accounts
  can be used at once.

## M2: CLI and UX
- [x] `agentswap` with subcommands: add, list, switch, remove, usage, auto
  (on/off/threshold), run, status. Pretty and --json output.
- [x] `agentswap waybar`: a streaming JSON line for a waybar custom module
  (active account per provider plus remaining quota), with no tight polling;
  respect the usage cache and backoff.

## M3: Nix
- [x] Flake: package (crane or buildRustPackage), devShell (with gitleaks and
  hooks installed), checks (cargo test, clippy -D warnings, fmt, gitleaks),
  and homeManagerModules.default (enable, package, autoswitch settings, waybar
  module option). The module must not write into ~/.claude or ~/.codex files
  that Home Manager manages.

## M4: Docs and release
- [x] README with attribution to claude-swap (MIT notice kept), install, usage,
  a security notes section, and a ToS note: using multiple accounts to avoid
  usage limits may be against provider terms; users are responsible.
- [x] LICENSE (MIT) plus a NOTICE crediting claude-swap.

## Questions
