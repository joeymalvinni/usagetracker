# Codex

Codex is on by default.

## Accounts

Codex supports as many profiles as you like. Each one uses its own `CODEX_HOME` (or a specific `auth_path`), and its real identity is the provider's `account_id`. If two profiles point at the same account, the first enabled one wins, and an existing profile can't quietly swap to a different account. Display names are just local labels.

## Where credentials come from

For each profile, UsageTracker reads `auth_path` if you've set it, otherwise `<codex_home>/auth.json`. The legacy default is `$CODEX_HOME/auth.json` or `~/.codex/auth.json`. Whichever file it lands on has to contain a non-empty access token and account ID.

## How usage is collected

1. Start `codex app-server` in the profile and ask it for the account identity, rate limits, reset credits, and account-wide usage.
2. If the app-server can't give you rate limits, ask `https://chatgpt.com/backend-api/wham/usage` using the bearer token and account ID you already have.
3. If account-wide activity isn't available at all, keep the history you already have and lean on local session logs for activity and cost estimates.

Rate-limit trouble can fall through to WHAM, but local logs never stand in for real, provider-reported quota.

## How the numbers are normalized

Provider windows become percent, credit, or amount windows, each with a stable ID and a UTC reset time. The daily buckets from `account/usage/read` are account-wide. Tokens from your local logs are treated as this-Mac observations, and their cost is estimated from a bundled, versioned price catalog — models that aren't in the catalog stay clearly marked as unpriced.

## Refresh timing and rate limits

Refreshes happen at most once a minute. A 429 from the provider starts a shared backoff of 5, 10, 20, 40, then 60 minutes. Local file activity can trigger a (coalesced) refresh, but it can't jump the backoff queue.

## What's kept in diagnostics

Diagnostics can note the collection mode, plan and email, reset-credit summaries, profile ID, account activity summaries, local cost coverage, model names, and the price-catalog version. They never include raw app-server or WHAM payloads, or your bearer token.

## What failures mean

- Auth file missing or unreadable → `credentials_missing` or `credentials_invalid`.
- HTTP 401/403 → `unauthorized`; a 429 → `rate_limited`.
- Transport trouble → `network`; a provider surface UsageTracker doesn't support → `provider_unavailable`; shapes it can't read → `parse`.
- When both the app-server and WHAM fail, you get the more informative of the two, with both paths described safely.

## A few security notes

UsageTracker launches the Codex executable you've configured, pointed at the profile's home, and reads its known session roots. Separate homes keep managed accounts isolated. A profile marked `owns_default_codex_activity` may additionally read `~/.codex/sessions` — and only one profile can own that.

## Tests and fixtures

Inline tests cover credential parsing, duplicate identities, app-server and WHAM normalization, reset credits, activity, local-log attribution, and price coverage. `just fixture` runs normalized Codex data through the real socket and Swift UI.

## Known limitations

- Local cost is an estimate, not a billing statement.
- Local logs without valid timestamps count toward lifetime and per-model totals, but not toward any dated total.
- Daily provider history is kept until you permanently delete the account; normalized snapshots are capped at 90 days and 10,000 rows per account.
