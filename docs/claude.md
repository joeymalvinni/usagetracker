# Claude

Claude is off by default — turn it on when you're ready.

## Accounts

Claude supports as many profiles as you like. Each managed account gets its own `CLAUDE_CONFIG_DIR` directory and its own Keychain services, so accounts never bleed into each other.

An account's real identity is its Anthropic `account.uuid`. (For older tokens that don't carry the profile scope, UsageTracker will fall back to a narrowly scoped cached UUID.) Emails, macOS usernames, plan tiers, and labels are just for display — they never decide who an account is. If two profiles share a UUID, the first enabled one wins; if a profile's UUID changes out from under it, that profile is rejected.

## Signing in from the app

Use Connect account to find an existing sign-in, or Add another account in Settings to start a new managed login. Reconnect is offered when the provider rejects an existing account. If Claude's browser flow displays an authentication code, expand “Enter a code (if shown)”, paste it into the secure Authentication code field, and submit it. If no code is shown, finish browser sign-in and wait for the account to connect automatically. The daemon forwards the code to the waiting Claude CLI, clears cached credentials after the CLI succeeds, and refreshes Claude usage. Repair is confirmed only after healthy usage is observed.

If macOS blocks the saved credentials, onboarding and Settings show Allow access so you can authorize the tracker without repeating browser sign-in. A pending account retains its profile ID before discovery succeeds, so this action authorizes that profile even when other Claude accounts are already connected.

Cancel terminates the waiting login process. A new login replaces the previous attempt for that provider, and unfinished attempts expire after ten minutes. Codes are bounded single-line input and are redacted from daemon request debug output. If the CLI stops accepting input, restart sign-in.

## Where credentials come from

UsageTracker looks for your Claude credentials in this order:

1. **The profile's macOS Keychain item** — either the legacy `Claude Code-credentials` entry or the service Claude Code derives from its config directory. You can point it somewhere else with `keychain_service` and `keychain_account`.
2. **The profile’s credentials file** (`credentials_file`, otherwise `.credentials.json` inside its config directory) — when the Keychain item is missing or cannot be accessed silently. The legacy default profile uses `~/.claude/.credentials.json`; another profile never implicitly borrows that file. Invalid Keychain JSON does not select a different source. If no fallback file exists, the original permission error is preserved.

Token refresh uses the same Keychain-to-file fallback policy as initial loading: a blocked Keychain read does not prevent refreshing the selected file’s token.

Both the OAuth access and refresh tokens have to be present. If a token is within 60 seconds of expiring, UsageTracker refreshes it through `POST https://platform.claude.com/v1/oauth/token` and writes the new one back to wherever it came from.

## How usage is collected

1. Look up your profile at `GET https://api.anthropic.com/api/oauth/profile`, then ask for usage at `GET https://api.anthropic.com/api/oauth/usage`.
2. If that returns a 401 or 403, refresh the OAuth token once and try again.
3. On network, provider, parsing, or Keychain-access failures — with `cli_enabled` — read `claude /usage` through a bounded terminal. If the TUI cannot provide usage, try `claude -p /usage --output-format json --no-session-persistence`. Rate limits and rejected sign-ins never start another fallback.

The CLI fallback runs with the profile’s own `CLAUDE_CONFIG_DIR`. When credentials are readable, its access token is supplied in `CLAUDE_CODE_OAUTH_TOKEN`. When tracker Keychain access is blocked, Claude Code may use its own access to that exact profile's native Keychain item. Custom service/account overrides cannot silently select another CLI sign-in. Competing inherited authentication settings are removed. Native-auth collection checks the cached profile UUID before and after the command; account storage also rejects identity changes. Discovery may use the selected profile's cached UUID on eligible failures only when the credentials come from that CLI profile’s native Keychain item or native credentials file. Custom service/account overrides and non-native credential files require an identity from the OAuth profile API; they never borrow the default CLI home’s cached UUID.

Each child is capped at twenty seconds and bounded output. The TUI process group is terminated after reading the meters or on timeout. Its hooks and MCP servers are disabled. The probe runs in a private, empty `usage-probe` directory owned by UsageTracker, never the daemon's inherited working directory or a user's project. Before initializing missing CLI UI state, UsageTracker checks the selected profile with `claude auth status --json`; only an existing Claude subscription sign-in can complete this initialization. It preserves account identity and all existing settings, and grants workspace trust only to its own probe directory. The same initialization runs before opening a managed session. There is no manual first-run setup step or replacement browser login just to collect usage. Real sign-in failures remain reconnect errors. Claude's `--no-session-persistence` flag is print-only, so it is used only for JSON mode. Missing/invalid credentials and rejected sign-ins still require their normal recovery actions. Tokens never appear in command arguments or logs; raw CLI error output is not logged. Anthropic documents the [OAuth environment variable](https://code.claude.com/docs/en/env-vars) for automated authentication.

## How the numbers are normalized

OAuth's canonical `limits` list becomes windows clamped to `0..100`, including scoped model limits such as Fable. Stable IDs preserve the legacy five-hour and seven-day window identities, and reset times are parsed into UTC. Older responses without `limits` still use recognized time-window utilization fields. Opaque fields such as `nimbus_quill` and dollar-denominated objects are not quota meters; cached placeholder meters are also filtered from the app. Model scope IDs are presented as public names such as Fable 5.1, while window IDs remain stable. Any extra usage shows up as a credit/spend window. The CLI fallback's parser understands Claude's own session and weekly usage text and its reset-time formats.

Your local JSONL history is used only for token activity and estimated cost on *this* Mac — never for quota percentages. It's found via `project_roots`, `<claude_config_dir>/projects`, or whichever single profile owns the shared default Claude project roots.

The September 9, 2026 bundled catalog includes Fable 5.1, Opus 5, Sonnet 5, and Haiku 4.5 using the [official model prices](https://platform.claude.com/docs/en/models/overview). Cache-write totals include the one-hour subset once; its separate rate affects estimated cost, not the token total. Unknown models are named in pricing coverage. Local logs are reconciled at startup as well as after file changes so upgrades can reprice existing history even when remote credential access is unavailable.

## Refresh timing and rate limits

Refreshes happen at most once a minute. Changes to your local JSONL files are debounced for 30 seconds and can trigger at most one refresh per minute. Account-wide polling is always the source of truth. A 429 puts the provider into shared backoff and, again, never switches to the CLI.

## What's kept in diagnostics

The provider page displays the credential subscription type, falling back to `subscriptionType` or `seatTier` from the selected profile’s cached `oauthAccount` only when its account UUID matches the collected account.

Diagnostics can note things like the collection mode, profile ID, Keychain service and account names, subscription tier, token expiry, scopes, the safe shape of a response, CLI fingerprint counters, and how much of your local cost could be priced. They never include OAuth response bodies, CLI output text, or your access and refresh tokens.

## What failures mean

- Keychain item or file missing → `credentials_missing`.
- Background Keychain operations disable macOS interaction in an isolated helper. Permission is requested only by the explicit Allow access action, once for the selected profile; cancellation or rejection never retries the prompt.
- Required or denied Keychain permission, or another Keychain read/write failure without a usable profile file → `keychain_access_failed`.
- Bad credential JSON, shape, or token fields → `credentials_invalid`.
- OAuth 401/403 or `invalid_grant` → `unauthorized`; a 429 → `rate_limited`.
- HTTP or CLI transport trouble → `network` or `provider_unavailable`; usage output it can't read → `parse`.
- When both OAuth and the CLI fail, the final message safely mentions both.

## A few security notes

Background reads and writes cannot show macOS permission dialogs. Silent helper work has a five-second deadline; an explicit permission request has a sixty-second deadline and never starts browser login. Successful reads and permission failures are coalesced in memory and silently revalidated after sixty seconds, so external sign-in changes are observed without restarting. A previously accepted value remains usable when silent revalidation requires interaction or returns an authentication failure, until provider authentication definitively rejects it. Rejected refresh tokens and repeated OAuth authentication failures invalidate both the collector and broker caches; a fresh silent read surfaces permission recovery when the replacement credential is protected. Refresh and launch perform fresh silent checks without discarding an “Allow once” value first. Writes through the broker update or invalidate cached values. Claude’s own credential cache also revalidates after sixty seconds.

Before opening a managed profile session, UsageTracker checks fresh credentials and refreshes expired tokens with guarded writes. File-sourced credentials only create a missing Keychain item; they never overwrite a competing login or refresh. Launch invalidates the collector credential cache. If synchronization fails, the app reports the appropriate permission, connection, or reconnect action instead of automatically opening browser login. Managed login and launch commands only ever see their own profile directory — use the app's per-profile launch action so activity gets attributed correctly. Managed accounts can also save a per-account working directory and structured launch flags (model, effort, dangerously-skip-permissions). The generated launcher refuses to run if the working directory no longer exists, and the dangerous flag is use-once unless you explicitly remember it. The launcher file on disk keeps the most recent session's flags until the next open, so re-running it from Finder repeats them. Your local history may contain project paths and model names, but UsageTracker doesn't copy whole records into its own storage.

## Comfort-pack import

Managed Claude accounts can import a scrubbed comfort pack from your default Claude home (`~/.claude` and `~/.claude.json`) into the account's isolated config directory. The import sheet calls `preview_account_import` for paths, default toggles, and size hints, then `import_account_data` to start a background job you poll with `get_import_job`.

v1 imports only the comfort toggles: scrubbed `settings.json` prefs, project trust flags from `.claude.json`, and prompt history. Stretch toggles such as plugins, transcripts, file history, tasks/teams, and sessions are listed in the preview but rejected if requested. `prefs_only` replaces selected preferences/history and merges only allowlisted trust fields into the destination `.claude.json`, preserving its identity, MCP configuration, and other account data; `replace` also removes paths recorded in the previous import manifest before copying again. Import never touches credentials, Keychain state, or project transcripts (which would confuse local usage attribution).

## Tests and fixtures

Inline tests cover the Keychain/file rules, token refresh, identity, OAuth and CLI parsing, reset times, local cost, project roots, and duplicate profiles. `just fixture` runs normalized Claude data all the way through the socket and UI.

## Known limitations

- If you write a multi-profile config by hand, CLI fallback starts on only for the first profile unless you set it on the others.
- Shared default activity is auto-assigned only when there's exactly one active managed profile to assign it to.
- Local cost is an estimate, and it can be partial when a model isn't in the price catalog.
