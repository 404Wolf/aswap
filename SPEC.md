# Codex usage-reset fallback: independent verification

Status: research complete; findings for Wolf's review. No implementation authorized or performed.
Bead: asw-dyc. Branch: wolf/codex-reset-fallback. PM: t2kg. Date: 2026-11-22.

## Scope and safety

PM t2kg changed the task from designing a new feature to independently auditing the existing feature. This document is that audit and the proposed acceptance contract for any later repair. All application/account interaction was read-only source inspection. No Codex app-server was launched, no provider API was called, no reset was triggered, and no secrets/token files were read. The upstream public source was fetched into a temporary research checkout. Only existing reset unit tests using synthetic `Fake` ports and temporary stores were selected; no real-account tests.

Audited aswap: `c0ff893b1eef31c77d7b1420cc2ea4e949905926`, package 0.2.1 (`Cargo.toml:1-4`). Upstream Codex: `104d2564181d8eb4e67d0c92005f3ab1052051d4`. Upstream citations below are immutable GitHub source links. Local citations refer to the audited aswap revision. This does not certify a particular installed Codex binary or VM configuration; neither was exercised.

## Plain answer to Wolf

**Yes: Codex exposes user-facing usage-reset redemption and a non-interactive app-server method that an external controller can automate.** Its TUI requires an explicit selection and confirmation; the app-server method itself does not require TUI interaction. This establishes the Codex side of the comparison with Claude; this audit did not independently research Claude's reset capabilities. It does not establish that Codex automatically spends resets by default, or that any particular account has an available/eligible reset.

aswap already implements opt-in automation. It is not a complete match for the requested contract: unknown eligibility is collapsed into unavailable, the history is not append-only, uncertain attempts can lose their recovery key, and there is no redemption-specific wake signal. Weekly-only selection also needs Wolf's interpretation of “NEXT scheduled reset.” Do not build another redemption path.

## Codex evidence and supported interaction

1. **User-facing entry:** `SlashCommand::Usage` opens the menu ([tui/src/chatwidget/slash_dispatch.rs:572](https://github.com/openai/codex/blob/104d2564181d8eb4e67d0c92005f3ab1052051d4/codex-rs/tui/src/chatwidget/slash_dispatch.rs#L572)). The menu presents “Redeem reset”; authenticated ChatGPT accounts see a positive count, unknown count is “Check availability,” and zero/non-ChatGPT disables it ([tui/src/chatwidget/usage.rs:32-64](https://github.com/openai/codex/blob/104d2564181d8eb4e67d0c92005f3ab1052051d4/codex-rs/tui/src/chatwidget/usage.rs#L32)). Selecting an offer opens confirmation, creates a UUID, and only the affirmative action emits consumption; the default selection is the negative choice ([usage.rs:218-275](https://github.com/openai/codex/blob/104d2564181d8eb4e67d0c92005f3ab1052051d4/codex-rs/tui/src/chatwidget/usage.rs#L218)). Herdr can send keys/text into a terminal (`hc send` / `hc keys` per `hc --help`), but no such interaction was performed. Automation should use the typed app-server surface rather than screen-driving confirmation.

2. **Non-interactive method:** `account/rateLimitResetCredit/consume` is a registered request, serialized in the `account-auth` group ([app-server-protocol/src/protocol/common.rs:1483-1486](https://github.com/openai/codex/blob/104d2564181d8eb4e67d0c92005f3ab1052051d4/codex-rs/app-server-protocol/src/protocol/common.rs#L1483)). The TUI calls precisely this typed request ([tui/src/app/background_requests.rs:912-927](https://github.com/openai/codex/blob/104d2564181d8eb4e67d0c92005f3ab1052051d4/codex-rs/tui/src/app/background_requests.rs#L912)). Parameters: required idempotency key (reuse for retries), optional opaque credit ID (omission lets the backend choose) ([protocol/v2/account.rs:443-450](https://github.com/openai/codex/blob/104d2564181d8eb4e67d0c92005f3ab1052051d4/codex-rs/app-server-protocol/src/protocol/v2/account.rs#L443)). No separate unattended `codex reset` shell command is needed. aswap already starts `codex app-server --listen stdio://`, initializes it, then sends a selected method (`src/usage.rs:557-574,669-684`); its reset adapter supplies both identifiers (`src/reset.rs:883-905`). These are source observations, not executed requests.

3. **Availability is not eligibility:** the read response optionally includes a summary; detail rows may be null (count only), empty (details fetched, none returned), or shorter than the count due to capping ([protocol/v2/account.rs:371-397](https://github.com/openai/codex/blob/104d2564181d8eb4e67d0c92005f3ab1052051d4/codex-rs/app-server-protocol/src/protocol/v2/account.rs#L371)). Rows expose ID, reset type, status, grant/expiry timestamps, title and description; unknown type/status are explicit enum values ([account.rs:403-437](https://github.com/openai/codex/blob/104d2564181d8eb4e67d0c92005f3ab1052051d4/codex-rs/app-server-protocol/src/protocol/v2/account.rs#L403)). The TUI distinguishes load errors from no resets ([usage.rs:133-149](https://github.com/openai/codex/blob/104d2564181d8eb4e67d0c92005f3ab1052051d4/codex-rs/tui/src/chatwidget/usage.rs#L133)) and can offer backend-selected redemption if details are absent ([reset_credits.rs:20-75](https://github.com/openai/codex/blob/104d2564181d8eb4e67d0c92005f3ab1052051d4/codex-rs/tui/src/chatwidget/reset_credits.rs#L20)). A positive available count does not prove a quota window is eligible. Public client code does not define the backend's full eligibility rules or idempotency retention duration.

4. **Outcomes:** `reset` consumes a credit and resets eligible windows; `nothingToReset` means no window is eligible; `noCredit` means no earned reset is available; `alreadyRedeemed` means that idempotency key already succeeded ([account.rs:463-471](https://github.com/openai/codex/blob/104d2564181d8eb4e67d0c92005f3ab1052051d4/codex-rs/app-server-protocol/src/protocol/v2/account.rs#L463)). The response contains only an outcome, not replacement usage. The TUI refreshes after success ([app/event_dispatch.rs:2501-2515](https://github.com/openai/codex/blob/104d2564181d8eb4e67d0c92005f3ab1052051d4/codex-rs/tui/src/app/event_dispatch.rs#L2501)). The contract says “eligible windows”; it does **not** promise that both weekly and five-hour windows always reset together. aswap's stronger wording in `src/reset.rs:3-8` and `README.md:110` is not established by this contract.

5. **Server/backend boundary:** app-server validates nonempty identifiers, requires backend-backed authentication, forwards to the backend with a timeout, and maps the four outcomes ([app-server/src/request_processors/account_processor/rate_limit_resets.rs:45-114](https://github.com/openai/codex/blob/104d2564181d8eb4e67d0c92005f3ab1052051d4/codex-rs/app-server/src/request_processors/account_processor/rate_limit_resets.rs#L45)). It contains no TUI-confirmation or local all-accounts-exhausted requirement. Backend client constructs the POST using the caller's redemption ID and optional credit ID ([backend-client/src/client/rate_limit_resets.rs:93-125](https://github.com/openai/codex/blob/104d2564181d8eb4e67d0c92005f3ab1052051d4/codex-rs/backend-client/src/client/rate_limit_resets.rs#L93)). These are implementation evidence only: direct backend calls are outside the permitted integration and were never made.

## Requirement audit and findings

### F1 — P1: unknown credit availability is reported as no credit

`src/usage.rs:101-112` maps null/missing detail rows to an empty vector, losing the upstream null-versus-empty distinction. `src/reset.rs:360-384` uses `available_usage_resets.unwrap_or(0)` and returns `Hold::NoCredit` when no row qualifies. Existing tests deliberately equate unknown count and absent details with no allowance (`src/reset.rs:1191-1207`). Thus a count-only offer or failed discovery looks unavailable, and an unknown farthest-reset account can be skipped in favor of a nearer known account.

Required repair: preserve unknown count/details/type/status separately from known unavailable; report a distinct eligibility/discovery hold and refresh through the supported read surface. Never consume to probe eligibility. If an unknown account could outrank the chosen candidate, defer selection. Backend window eligibility may remain unknown until an otherwise authorized attempt; do not describe a listed credit as guaranteed eligible.

### F2 — P1: pending recovery identity is discarded before outcome is known

The normal path correctly persists a UUID and credit ID before sending (`src/reset.rs:600-615,715-719`) and reuses them on retry. Atomic state writing syncs both file and directory (`src/store.rs:443-461`). However, after three uncertain responses, it clears `pending`, and the history record intentionally excludes key/credit identity (`src/reset.rs:79-99,774-790`). Window passage, removal, changed eligibility and expiry also abandon pending state (`src/reset.rs:207-225,623-687`). A successful request whose reply was lost can become merely `gave-up`/`unconfirmed`; the key needed to obtain `alreadyRedeemed` is gone. A cooldown/skip limits follow-up spending but does not reconcile the spent reset or guarantee the waiting agent benefits from it.

Required repair: retain unresolved intent and its identifiers durably, including after retry budget exhaustion. Pause sending after the attempt cap, but preserve reconciliation state. Never replace an unresolved logical redemption with a new key merely because time passed or retries failed. Account removal/credential replacement needs explicit safe reconciliation semantics rather than silently forgetting the key. This audit does not claim the backend spends twice for a reused key.

### F3 — P1: required append-only journal does not exist; control updates race redemption

`ResetLog` is one mutable JSON snapshot; `push` prunes history to 50 records (`src/reset.rs:170-182,845-851,908-910`). There is no append-only durable intent/outcome journal. The reset lock serializes ticks sharing one store root (`src/reset.rs:948-985`), but `set_mode` takes only the store lock and can clear pending state while a tick holds an old in-memory snapshot (`src/reset.rs:854-875`; caller `src/main.rs:606-609`). Turning off during a blocked pre-read can therefore return before the stale tick persists/sends; a later save can also overwrite the off path's abandonment record. This is a source-derived interleaving, not a live experiment. The lock is store-scoped, not globally exclusive across independent store roots or hosts. Owner-only refresh is a useful additional gate, not distributed exactly-once evidence.

Required repair: one consistent reset-operation locking/order or generation protocol for mode changes, intent persistence, and dispatch; recheck authorization at dispatch. Append and fsync immutable intent/attempt/outcome events, reconstruct state on startup, and keep unresolved identifiers. Define the single authoritative store/refresh owner scope; do not claim stronger cross-store/host guarantees than provided.

### F4 — P2: exhaustion and ranking implement weekly-only policy

`guard` requires every stored Codex account to have fresh known weekly usage at 100% (`src/reset.rs:297-322`). It includes reserved accounts and accounts unusable for other reasons (`src/runner.rs:1751-1777` returns every stored account); a reserved account with room blocks resetting usable accounts, intentionally tested (`src/reset.rs:1097-1111`). A full five-hour window with weekly room is explicitly not considered exhausted (`src/reset.rs:1115-1122`). This is conservative about spending but differs from “all usable accounts exhausted.” Only the chosen account gets a forced fresh read; other accounts use caches (`src/reset.rs:573-599`, `src/usage.rs:29,154`).

Ranking uses `weekly_reset`, descending, then soonest credit expiry and label (`src/reset.rs:385-397`; `src/usage.rs:178-194`). It satisfies **weekly** 7 days beats 3 days. It does not maximize each account's literal next scheduled reset: A(weekly 7d, session 1h) beats B(weekly 3d, session 4h), though B's next reset is farther away. For strictly weekly-exhausted accounts, session-only resets do not recover usable quota, so weekly may be the intended recovery boundary.

Wolf decision before repair: define “NEXT” as the earliest scheduled window boundary or the next boundary that restores usable quota; define whether reserved/auth-invalid/model-ineligible accounts count. Then use that same definition for exhaustion, candidate ranking, and the 7d/3d fixture. Unknown usable accounts must defer spending. No interpretation has been implemented in this audit.

### F5 — P1: redemption is not connected to prompt wake/resume

Reset checks run in `auto once/watch` before normal selection (`src/main.rs:643-670`). `src/runner.rs` does not call reset tick; a supervised waiting session alone cannot redeem without the separate monitor. A successful redemption refreshes quota (`src/reset.rs:748-764`), and waiting sessions eventually poll/reselect (`src/runner.rs:713-748,1438-1470`) and resume (`src/runner.rs:785-803,850`). There is no reset-specific signal or redemption generation.

Worse, a rejected account remains held for 15 minutes, doubling up to 4 hours, unless a previously scheduled reset timestamp has passed (`src/runner.rs:1493-1522`). An earned reset redeemed before that boundary does not release this hold. Polling finds fresh headroom but can still reject it, delaying benefit after consumption. Existing wake tests cover scheduled resets, not redeemed credits.

Required repair: publish durable verified recovery after success/reconciliation and fresh usable quota; wake waiting supervisors and let that verified generation release the corresponding rejection hold. Preserve conversation/resume args, identity checks and restart/switch gates. Failed post-reset reads keep the session waiting without authorizing another credit. Polling remains a fallback, with a documented bounded latency.

### F6 — P2: current upstream recovery permission is ignored

The audited upstream read contract says `ordinaryUsageAllowed` is backend permission and null is unavailable; clients must not infer recovery from percentages/times alone ([account.rs:371-374](https://github.com/openai/codex/blob/104d2564181d8eb4e67d0c92005f3ab1052051d4/codex-rs/app-server-protocol/src/protocol/v2/account.rs#L371)). aswap's `Usage`/Codex parser retains windows, money credits and reset credits, but not that field (`src/usage.rs:29-39,473-482`); recovery checks percentages (`src/runner.rs:1511-1522`). Consequently a reset outcome plus fresh low percentages cannot establish the newest protocol's recovery permission. This is a source-version compatibility finding, not evidence about the installed binary's field support.

Required repair: version-aware permission handling; preserve unknown versus denied, and require authoritative recovery evidence on versions providing it. Include null/false/true fixtures and identity validation.

## What already works

Default reset mode is off (`src/store.rs:73-85`). Dry-run performs no redemption (`src/reset.rs:547-572`). Non-Codex and refresh followers are rejected (`src/reset.rs:529-540`). A nonblocking exclusive file lock spans a tick (`src/reset.rs:948-985`). A pending operation blocks choosing a new one, persists identifiers before dispatch, retries the same identifiers, and treats `alreadyRedeemed` as success (`src/reset.rs:541-542,600-615,696-703,715-764`). Money wording halts opt-in execution (`src/reset.rs:804-839`). These are valuable controls to retain.

## Acceptance checks for a later authorized change

Fixtures/mocks only; never redeem against a real account, including in manual acceptance.

- Unknown usage/eligibility, missing details, reserved and unusable accounts; no spend until the agreed usable-account exhaustion predicate is proven.
- 7d versus 3d and crossed session/weekly schedules under Wolf's chosen NEXT definition; expiry tie-breaks.
- Two concurrent controllers and concurrent reset-off/settings changes; single durable intent and dispatch authority.
- Crash at each persistence/send/result boundary; timeout after backend success; restart reuses the exact identifiers and reconciles `alreadyRedeemed` without a second credit.
- Retry cap retains unresolved journal intent; no-credit/nothing-to-reset remain separate terminal non-spend outcomes; malformed/errors remain uncertain.
- Append-only journal replay, fsync failures, torn final event and snapshot recovery without forgetting an unresolved attempt.
- Reset success followed by failed read, then verified recovery: waiting agent wakes and resumes once, preserves its conversation, and does not remain blocked until the old scheduled boundary. Ordinary-usage permission false/null must not masquerade as recovery.

Validation: `cargo test --lib reset::tests --offline` passed all 23 selected tests (0 failed); `git diff --check` passed. The selected tests use synthetic ports and temporary stores. No feature code changed. No PR opened. Any later implementation/PR requires Wolf's go and is held for Wolf.
