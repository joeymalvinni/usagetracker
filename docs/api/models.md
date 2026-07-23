# Model reference

The generated [request](schemas/v3/request.json) and [response](schemas/v3/response.json) schemas are the authority on names, types, required fields, enum values, and nullability. This page fills in the meaning the schema can't capture.

## Conventions

- `ProviderId`, `AccountId`, and `RefreshJobId` are opaque, case-sensitive strings. Don't parse or build them yourself.
- The supported provider IDs are `codex`, `claude`, `cursor`, `opencode_go`, and `grok`.
- Timestamps are RFC 3339 instants in UTC; date buckets are `YYYY-MM-DD` local-calendar dates.
- Percent fields run `0..100`, not `0..1`. Amount units are `tokens`, `requests`, `credits`, `usd`, `percent`, or `unknown`.
- A missing optional collection with a documented Serde default means `[]`; everywhere else, use the schemas to tell required, omitted, and `null` apart.
- Ignore object fields you don't recognize. `diagnostics` is optional, opaque, provider-specific JSON — not a stable field-level contract.

## Accounts and providers

`Account.id` is UsageTracker's own stable management ID. `external_account_id` is the provider's identity, and `profile_id` marks which local credentials the account is isolated to. `display_name` is a local, provider, or generated label — it never establishes identity. `hidden` controls visibility; `collection_enabled` controls collection.

`ServerInfo` reports the protocol capabilities and an ordered list of `ProviderDescriptor` values. A provider's capabilities tell you whether the account-add, repair, launch, or workspace methods even apply to it. `minimum_refresh_interval_seconds` is the fastest cadence a provider will collect at — currently 60.

## Usage

`UsageSnapshot` is the latest normalized observation for one provider/account. `collected_at` is when the provider was collected, not when the response was sent. `windows` keep the provider's order, so identify a window by its `(provider_id, account_id, window_id)` rather than by label or position.

`UsageWindow` can express absolute amounts, percentages, or both. A missing value means the provider didn't supply or support it — don't infer a limit from a field that isn't there. `reset_at: null` means no reliable reset is known. `kind` is a presentation-level grouping, not a billing guarantee.

`UsageWindowProvenance` tells you whether a window is account-wide or local, authoritative or estimated, complete or partial, and quota-like or not. Only `authoritative: true` *and* `quota_like: true` is safe to drive quota alerts.

`UsageDataSource` is one of `provider_reported`, `local_logs`, `local_database`, or `synthetic_local_estimate`. Scopes are `account_wide`, `organization`, `this_device`, `selected_local_roots`, and `workspace`. Organization-scoped windows can include other members and are not personal quota-alert inputs. Quality, completeness, and confidence are all separate axes — don't collapse them.

## Dashboard and forecasts

`UsageDashboardSummary.accounts` is ordered by provider then account, and `days` runs ascending by date. Cross-account totals carry `AggregateProvenance`, because mixed scopes aren't equivalent billing records.

`DailyUsagePoint.tokens` is observed activity. `cost_usd` may be absent. `priced_tokens` and `unpriced_tokens` explain how much of the cost could be priced — unpriced tokens are not free tokens. `PricingCoverage.covered_percent` is priced tokens divided by priced-plus-unpriced tokens.

`CostSummary.models` contains provider-reported model totals when available. Vendor cost, provider fees, metered cost, and chargeable-only cost remain separate fields rather than being inferred from one another.

`UsageEventPage` is a bounded offset page for one account. Events are ordered by occurrence time and stable event ID, newest first. Event IDs are opaque normalized identities; clients must not parse them.

`UsageForecast` is built from your retained observations, not from provider guidance. Its identity is provider/account/window, and a nullable projection means there wasn't enough data (or it doesn't apply). Status is one of `insufficient_data`, `safe`, `on_pace`, `at_risk`, or `exhausted`; confidence is `low`, `medium`, or `high`.

## Health and refresh results

`StateSnapshot.connectivity` is transient machine-wide reachability, separate from durable provider health. `offline` means macOS reports no usable default network route, `online` means a route is available (not that every provider is healthy), and `unknown` means reachability could not be determined. `changed_at` is when the daemon last observed a status transition. No external connectivity probe is sent.

`ProviderHealth` is the durable latest state, and it can hold both the last success and last failure times at once. `last_error_*` is sanitized operational text, not an API error object. Health statuses are `ok`, `credentials_missing`, `auth_failed`, `rate_limited`, `provider_error`, `parse_error`, `backing_off`, and `disabled`.

`ProviderRefreshResult` is one provider/account outcome inside a completed job. Its statuses are `ok`, `credentials_missing`, `credentials_invalid`, `unauthorized`, `rate_limited`, `network`, `parse`, `provider_unavailable`, `storage_error`, and `disabled`. A job can be `completed` even when some of these are failures. `RefreshJob.skipped_offline` identifies jobs where remote collection was skipped because machine-wide reachability was definitively offline; clients should use this job-scoped value instead of sampling connectivity again when presenting the outcome.

## Ordering

| Collection | Order |
| --- | --- |
| Server providers | Fixed: Codex, Claude, Cursor, OpenCode Go, Grok. |
| Accounts | `provider_id`, then `profile_id`, then `external_account_id`. |
| Latest snapshots | `provider_id`, then `account_id`. |
| Health | `provider_id`, then `account_id` (a provider-level row has no account). |
| Dashboard days | Ascending by date. |
| Pending notifications | Ascending by ID, at most 100. |
| Refresh provider results | Collector execution order — don't rely on it for identity. |
