# Codex collection

Codex support lives in the `crates/usage-daemon/src/providers/codex/` module tree and is enabled by
default.
Each configured profile runs in its own `CODEX_HOME`, which lets the daemon query multiple
ChatGPT/Codex accounts without changing the user's active terminal login.

Account identity comes from the authenticated Codex `account_id`, never from the profile's display
name. The same account id may only be connected once: when multiple profiles authenticate as the
same account, the first enabled profile in config order is canonical and later duplicates are not
collected. Storage enforces the same uniqueness rule and rejects a profile whose authenticated
account changes, preventing a reconnect from merging two accounts' retained history. To replace the
account behind a profile, permanently delete the old UsageTracker account first and add a new one.

## Collection paths

The primary collection path starts `codex app-server` for the profile and requests:

- `account/read` for account identity and plan metadata.
- `account/rateLimits/read` for current rate-limit windows and reset credits.
- `account/usage/read` for account-wide token activity.

`account/usage/read` is the authoritative activity source. It returns lifetime summary fields and
daily token buckets for the account, including activity produced on other computers. The daemon
stores every returned bucket in `provider_daily_usage`, keyed by provider, account, and date. Rows
are updated when Codex revises a day but are never removed merely because a later response omits an
older day. This preserves history beyond any upstream response-retention window.

The daemon keeps every retained row, but socket responses include only the latest 30 days under
`metadata.codex_activity.by_day`, which is the range consumed by the Swift charts. An incrementally
maintained SQLite summary keeps `lifetime_tokens` and `daily_bucket_count` exact without scanning or
serializing the entire history on every UI refresh. Summary fields from the latest provider response
remain under `metadata.codex_activity`, including peak daily tokens, streaks, and longest turn.

## WHAM fallback

If app-server rate-limit collection fails, the daemon requests
`https://chatgpt.com/backend-api/wham/usage` with the profile's bearer token and account id. WHAM
provides rate limits, plan metadata, credits, and reset credits; it does not provide daily or
lifetime token activity.

If only `account/usage/read` fails or is unavailable in an older Codex version, rate-limit
collection still succeeds. The daemon records a warning and uses local activity as a fallback while
continuing to serve the last successfully retained account-wide history.

## Local log estimates

Local session logs are not authoritative activity because they cover only one computer and do not
contain an account id. They remain useful for estimating model-level cost. For managed profiles,
the daemon scans the profile's own session directory and scans the standard `~/.codex/sessions`
directory only for the profile marked as its durable default-activity owner. On migration, the
daemon assigns that ownership once when exactly one managed profile's auth matches the default
Codex auth, then persists it as `owns_default_codex_activity`. Later changes to the default Codex
login do not move historical local activity between profiles.

Local cost metadata is stored under `metadata.codex_cost` with `estimate=true`, `partial=true`, and
`complete_lookback=false`. When account activity is available, local token counts are not added to
account token counts. The Swift dashboard uses account activity for tokens and local logs only for
directly calculated estimated spend. When a Codex account has account-wide daily tokens but no
local cost row for a day, the dashboard applies the effective cost-per-token observed from that
provider's visible local Codex logs. Only the scalar pricing reference is shared: dates and token
counts always come from the selected account's own account-wide history. This gives remote-only
profiles an approximate cost graph without pretending the account API supplied model or
input/output details. These values remain labeled as estimated and partial, and zero-cost local
rows are never replaced by the fallback.

Codex daily buckets and local cost rollups use UTC calendar dates so server-provided `startDate`
values and timestamped local events share one day boundary. Local events without a valid timestamp
remain in lifetime and model totals but are excluded from today, lookback, and per-day totals.

## Persistence

Migration `0002_provider_daily_usage.sql` creates the durable daily table. Permanent account deletion
also deletes that account's daily history. Hiding or disabling an account does not delete history.
Daily rows and lifetime aggregates are retained permanently. High-resolution normalized snapshots
are bounded to 90 days and 10,000 rows per account, and debug raw payloads are bounded to 100 per
account, preventing polling data from growing without limit.
