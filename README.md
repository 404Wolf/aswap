# aswap

aswap keeps several Claude Code, Codex CLI and pi logins on one Linux machine and
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

### One call, picked by quota

`--auto` runs the provider in a PTY, which suits an interactive session but
breaks `claude -p` pipes. `--pick` chooses the account the way `--auto` does at
launch, then runs with plain stdin/stdout/stderr and no supervision:

```sh
echo '{"type":"user",...}' | aswap run claude --pick -- -p --output-format stream-json
```

If the default account's quota is unknown because its login expired or its
reading failed, or its last reading is over the threshold, `--pick` takes the
best account whose last reading is below the threshold.

### pi with its own ChatGPT login

pi is a provider like the other two: an aswap pi account is one ChatGPT
subscription login in pi's `auth.json` (`openai-codex` or `openai`, Sign in
with ChatGPT).

```sh
pi            # /login with ChatGPT
aswap add pi personal
aswap run pi -- --model openai-codex/gpt-6.1-sol
aswap run pi --auto       # resume the session on another account at a limit
```

- `aswap add pi` moves the login out of pi's `auth.json`, leaving pi's other
  logins and API keys there. Two programs refreshing one rotating token
  break each other. `/login` in plain pi again for an independent login.
  The move happens under pi's own lock: the login is saved and checked in
  aswap first, and taken out of pi only then, so a failure leaves it in pi.
- An aswap pi run only has that ChatGPT login: pi's API keys and other
  logins, and `OPENAI_API_KEY`/`ANTHROPIC_API_KEY` in the environment, are
  not passed in (account isolation). The same holds for `--via pi`.
- `--auto` follows pi's own session changes (`/new`, `/resume`, `/fork`)
  through a small extension aswap loads with `-e`. It needs to know the
  session: start without a session option, or with `--session-id <uuid>`;
  `--session <path>` or `--continue` leave it unknown, and aswap then never
  restarts that run.
- A run gets a private `auth.json` holding an access-only copy of the
  login. pi's settings, sessions, extensions, themes and models are shared
  with your normal `~/.pi/agent` (or `PI_CODING_AGENT_DIR`). aswap refreshes
  the token; pi re-reads the file when it changes.
- Quota (`usage`, `status`, `list`, Waybar) is read for `openai-codex`
  logins, through Codex's app-server. Sign in with ChatGPT logins report no
  quota, so they show as unknown.
- `switch pi --activate-live` is refused: pi's default login stays in pi's
  hands.

### Claude inside pi

[pi-claude-code-provider](https://www.npmjs.com/package/pi-claude-code-provider)
lets pi use Claude by running the real `claude -p` once per turn, so turns
draw on the Claude plan. aswap can choose the account for it:

```sh
aswap run claude --via pi -- --model pi-claude-code-provider/sonnet     # account picked per turn
aswap run claude --via pi --account work -- --model pi-claude-code-provider/sonnet  # one account
```

- Without `--account`, pi's provider runs a private shim
  (`~/.local/share/aswap/bin/claude-pick`) as `claude`. The shim is
  `aswap run claude --pick`, so each turn picks its account by quota, and a
  turn after a limit or an `aswap switch` moves without restarting pi.
- With `--account`, pi runs on that account's launch view, and every turn
  uses that account.
- Arguments after `--` go to pi. Install the provider in pi yourself
  (`pi install npm:pi-claude-code-provider`, or `-e <path>`).
- The provider refuses a `claude` that always adds
  `--dangerously-skip-permissions`. If your `claude` on `PATH` is such a
  wrapper, set `ASWAP_CLAUDE_BIN` to the plain binary. `ASWAP_PI_BIN` names
  the pi binary.

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

## Development

```sh
nix develop                          # also installs the secret-scanning git hooks
scripts/isolated cargo test --locked
nix flake check
```

Tests use synthetic fixtures only. Never point them at real logins.

Using several accounts to avoid usage limits may break the providers' terms.
aswap is not affiliated with Anthropic or OpenAI.
