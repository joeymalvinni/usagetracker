# OpenCode Go

OpenCode Go is off by default. Its provider ID is `opencode_go` — plain `opencode` won't work.

## Accounts

OpenCode gives you one web workspace at a time, or a single local fallback identity. There's no managed multi-account support here. If you set a `workspace_id`, that becomes the account identity; otherwise UsageTracker picks the first workspace your session can reach.

## Where credentials come from

Cookie headers are resolved in this order:

1. `USAGE_TRACKER_OPENCODE_GO_COOKIE` (the legacy `USAGE_TRACKER_OPENCODE_COOKIE` still works), `cookie_header`, or `~/.usagetracker/opencode_go.cookie`.
2. A filtered header cached in the Keychain.
3. `auth` and `__Host-auth` imported from a supported Chrome-family, Dia, or Firefox store for `opencode.ai` / `app.opencode.ai` — then cached for next time.

The Keychain cache is updated only when the filtered header changes. Successful Keychain reads stay in memory for the daemon's lifetime, and an unchanged conditional write uses that in-memory value, so polling does not repeatedly prompt for the same item. Keychain operations share UsageTracker's serialized helper, so overlapping discovery and refresh work can't write the cache concurrently.

Workspace selection follows the same idea: a configured `workspace_id` first, then `USAGE_TRACKER_OPENCODE_GO_WORKSPACE_ID`, then automatic discovery.

## How usage is collected

1. Ask the authenticated OpenCode web console for your rolling, weekly, and monthly Go usage, your history, your workspace identity, and (if it's there) your Zen balance.
2. If a cached or imported cookie turns out to be bad, clear the cache, import once more, and retry. Manual cookies you set yourself are never bypassed this way.
3. If the web won't cooperate at all, read `~/.local/share/opencode/opencode.db` — but only when it actually has OpenCode Go auth and usage rows.

## How the numbers are normalized

On the web, a percentage means **used**, not remaining — so UsageTracker treats those as authoritative workspace windows with parsed reset times. Web and local history become observed activity and cost. The local SQLite fallback never makes up quota limits, percentages, or reset dates; anything from it is clearly marked as this-device, estimated, partial, and non-authoritative.

## Refresh timing and rate limits

Refreshes happen at most once a minute. A 429 from the web starts shared backoff. The local fallback can still report observed activity while web quota is unavailable, but it won't clear or replace the provider's rate-limit state.

## What's kept in diagnostics

Diagnostics can note the collection mode, workspace and email, cookie source name, Zen balance, how complete the history is and its row counts, the local database path, and local cost totals. They never include cookie values or raw web pages.

## What failures mean

- No cookie or local auth → `credentials_missing`; a manual cookie it can't use → `credentials_invalid`.
- Web 401/403 → `unauthorized`; a 429 → `rate_limited`.
- HTTP, browser, or SQLite access trouble → `network` or `provider_unavailable`; missing usage or workspace shapes → `parse`.
- When both paths fail, you get the web failure.

## A few security notes

Manual cookie headers are secrets — treat them that way. Browser import reads only the supported domains and cookie names, but it may need Keychain access for the browser's Safe Storage key. Requests only ever go to fixed OpenCode HTTPS hosts. The local fallback reads only the known OpenCode auth and database path.

## Tests and fixtures

Inline tests cover the used-vs-remaining percentage rule, workspace extraction, web and history parsing, the local SQLite variants, cookie filtering and import, and cost aggregation. `just fixture` runs normalized OpenCode data over the socket and UI.

## Known limitations

- Only one workspace identity is active at a time.
- Zen balance is best-effort — if it's missing, otherwise-valid usage collection still succeeds.
- The local fallback reports observed spend and activity only; it can't represent account-wide quota.
