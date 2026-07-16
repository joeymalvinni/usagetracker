# Claude

Claude is off by default — turn it on when you're ready.

## Accounts

Claude supports as many profiles as you like. Each managed account gets its own `CLAUDE_CONFIG_DIR` directory and its own Keychain services, so accounts never bleed into each other.

An account's real identity is its Anthropic `account.uuid`. (For older tokens that don't carry the profile scope, UsageTracker will fall back to a narrowly scoped cached UUID.) Emails, macOS usernames, plan tiers, and labels are just for display — they never decide who an account is. If two profiles share a UUID, the first enabled one wins; if a profile's UUID changes out from under it, that profile is rejected.

## Where credentials come from

UsageTracker looks for your Claude credentials in this order:

1. **The profile's macOS Keychain item** — either the legacy `Claude Code-credentials` entry or the service Claude Code derives from its config directory. You can point it somewhere else with `keychain_service` and `keychain_account`.
2. **A credentials file** (`credentials_file`, default `~/.claude/.credentials.json`) — but only when the Keychain item isn't there at all. Any other Keychain error stops the search rather than falling through.

Both the OAuth access and refresh tokens have to be present. If a token is within 60 seconds of expiring, UsageTracker refreshes it through `POST https://platform.claude.com/v1/oauth/token` and writes the new one back to wherever it came from.

## How usage is collected

1. Look up your profile at `GET https://api.anthropic.com/api/oauth/profile`, then ask for usage at `GET https://api.anthropic.com/api/oauth/usage`.
2. If that returns a 401 or 403, refresh the OAuth token once and try again.
3. If it still fails for any reason other than rate limiting — and you've set `cli_enabled` — fall back to `claude -p /usage --output-format json --no-session-persistence`.

The CLI fallback runs with the profile's own `CLAUDE_CONFIG_DIR` and is capped on both output size and time. A 429 (rate limit) never triggers the fallback — UsageTracker just waits.

## How the numbers are normalized

OAuth's canonical `limits` list becomes windows clamped to `0..100`, including scoped model limits such as Fable. Stable IDs preserve the legacy five-hour and seven-day window identities, and reset times are parsed into UTC. Older responses without `limits` still use their utilization fields. Any extra usage shows up as a credit/spend window. The CLI fallback's parser understands Claude's own session and weekly usage text and its reset-time formats.

Your local JSONL history is used only for token activity and estimated cost on *this* Mac — never for quota percentages. It's found via `project_roots`, `<claude_config_dir>/projects`, or whichever single profile owns the shared default Claude project roots.

## Refresh timing and rate limits

Refreshes happen at most once a minute. Changes to your local JSONL files are debounced for 30 seconds and can trigger at most one refresh per minute. Account-wide polling is always the source of truth. A 429 puts the provider into shared backoff and, again, never switches to the CLI.

## What's kept in diagnostics

Diagnostics can note things like the collection mode, profile ID, Keychain service and account names, subscription tier, token expiry, scopes, the safe shape of a response, CLI fingerprint counters, and how much of your local cost could be priced. They never include OAuth response bodies, CLI output text, or your access and refresh tokens.

## What failures mean

- Keychain item or file missing → `credentials_missing`.
- A rejected Keychain password is prompted again immediately, up to three attempts during the same refresh. Cancel stops without another prompt.
- Exhausted Keychain authentication or another Keychain read/write failure → `keychain_access_failed`.
- Bad credential JSON, shape, or token fields → `credentials_invalid`.
- OAuth 401/403 or `invalid_grant` → `unauthorized`; a 429 → `rate_limited`.
- HTTP or CLI transport trouble → `network` or `provider_unavailable`; usage output it can't read → `parse`.
- When both OAuth and the CLI fail, the final message safely mentions both.

## A few security notes

Reading or refreshing credentials in the Keychain can prompt macOS for permission. UsageTracker distinguishes a rejected Keychain password from Claude rejecting an OAuth token, and successful Keychain reads are cached in memory for the daemon's lifetime so discovery and collection do not repeat an accepted prompt. Writes made through UsageTracker update that cache; changes made by another process are picked up after the daemon restarts. Managed login and launch commands only ever see their own profile directory — use the app's per-profile launch action so activity gets attributed correctly. Your local history may contain project paths and model names, but UsageTracker doesn't copy whole records into its own storage.

## Tests and fixtures

Inline tests cover the Keychain/file rules, token refresh, identity, OAuth and CLI parsing, reset times, local cost, project roots, and duplicate profiles. `just fixture` runs normalized Claude data all the way through the socket and UI.

## Known limitations

- If you write a multi-profile config by hand, CLI fallback starts on only for the first profile unless you set it on the others.
- Shared default activity is auto-assigned only when there's exactly one active managed profile to assign it to.
- Local cost is an estimate, and it can be partial when a model isn't in the price catalog.
