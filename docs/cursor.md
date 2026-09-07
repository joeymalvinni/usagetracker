# Cursor

Cursor usage is collected from Cursor's signed-in web dashboard APIs. These are internal interfaces rather than a public personal-usage API, so UsageTracker validates their response shapes and keeps the last successful snapshot when a later refresh fails.

## Credentials and accounts

UsageTracker resolves Cursor sessions in this order:

1. `USAGE_TRACKER_CURSOR_COOKIE`, when explicitly set.
2. A validated, account-bound daemon-memory cache.
3. Cursor.app's `cursorAuth/accessToken` from its read-only `state.vscdb`.
4. Strictly allowlisted Cursor session cookies from supported browsers, when `USAGE_TRACKER_ALLOW_BROWSER_COOKIE_IMPORT=1`.

The desktop access token is decoded only to validate its user ID and expiration, then converted in memory to the `WorkosCursorSessionToken` cookie expected by Cursor's dashboard. It is re-read during collection so Cursor retains ownership of refresh and account switching. Tokens and cookie headers are never copied into UsageTracker config, SQLite, diagnostics, or logs.

Every candidate is validated against `/api/auth/me` and bound to its stable Cursor user ID. Collection only uses a credential matching the requested account. A rejected session is invalidated and reloaded once; rate limits, network errors, and server errors never cause fallback to another account.

## Usage

The required `GET /api/usage-summary` response supplies the billing cycle and plan data. Identity from `GET /api/auth/me` and legacy request quotas from `GET /api/usage?user=<id>` are best-effort during collection. UsageTracker also posts the billing-cycle range to `/api/dashboard/get-filtered-usage-events` and collects every bounded page of the authenticated user's detailed usage.

UsageTracker reports:

- `cursor_total` — included plan usage, an Enterprise/Team personal cap, or a clearly labeled team pool.
- `cursor_auto` — Cursor's Auto lane, when reported.
- `cursor_api` — named-model/API usage, when reported.
- `cursor_on_demand` — the personal on-demand budget.
- `cursor_team_on_demand` — a separate organization-scoped team budget when there is no personal budget.

Headline precedence is Cursor's total percentage, both lane percentages averaged, either lane, plan used/limit, personal overall used/limit, then the shared team pool. Cursor percentage fields are already percentage units: `0.36` means `0.36%`, not `36%`.

Legacy request plans replace the total money-based window with requests used and limited; Auto and API lanes are hidden for those plans.

Team pools and team budgets use the `organization` provenance scope. They can include other members and therefore do not drive personal quota alerts.

Detailed events are normalized into per-day and per-model token and cost totals. `tokenUsage.totalCents` is the underlying model/vendor cost, `cursorTokenFee` is Cursor's additional fee, and `chargedCents` is the provider-reported metered cost. Chargeable-only cost is reported separately so included-plan metered value is not presented as out-of-pocket spending.

Event pagination uses a fixed billing-period cutoff, bounded page and record counts, and a final first-page consistency check. An inconsistent, oversized, or malformed event feed is rejected as a whole; the last complete event history is retained while the required quota summary continues to refresh.

## Failures and privacy

- Missing Cursor.app login and disabled/unavailable browser import produce `credentials_missing`.
- A malformed desktop token produces `credentials_invalid`.
- HTTP 401/403 produces `unauthorized`; 429 produces `rate_limited`.
- Bounded transport failures produce `network` or `provider_unavailable`; unsupported response shapes produce `parse`.

Responses are bounded and parsed in memory. UsageTracker stores normalized windows, sanitized plan metadata, daily and model aggregates, and normalized individual events. Raw responses, formatted cost strings, owning-user/team identifiers, and credentials are not stored.
