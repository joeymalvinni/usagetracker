# Codex collection

Codex support lives in `crates/usage-daemon/src/providers/codex.rs` and is enabled by default.
Each configured profile runs in its own `CODEX_HOME`, which lets the daemon query multiple
ChatGPT/Codex accounts without changing the user's active terminal login.

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

The Swift app receives retained rows under `metadata.codex_activity.by_day`. It uses those rows for
token charts and totals. The summary fields from the latest response remain under
`metadata.codex_activity`, including lifetime tokens, peak daily tokens, streaks, and longest turn.

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
directory only when its active account matches that profile.

Local cost metadata is stored under `metadata.codex_cost` with `estimate=true`, `partial=true`, and
`complete_lookback=false`. When account activity is available, local token counts are not added to
account token counts. The Swift dashboard uses account activity for tokens and local logs only for
estimated spend.

## Persistence

Migration `0002_provider_daily_usage.sql` creates the durable daily table. Permanent account deletion
also deletes that account's daily history. Hiding or disabling an account does not delete history.
