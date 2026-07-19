# Method reference

Every method here landed in protocol v3. Each request needs `api_version: 3`, and unless noted, optional parameters accept both being left out and being `null`. Every response carries the same version and either the `type` listed below or `type: "error"`.

Reads have no side effects and are safe to retry. Storage-backed reads use SQLite snapshots wherever a combined view is needed. Mutations are serialized inside the daemon and persisted before you get a success back, so later requests see them — but provider-side login and launch effects aren't transactional.

First-party clients use these budgets: 3 seconds for reads, 5 for config/account updates and removal, 10 for refresh start plus add/delete/repair/launch, and 20 for provider setup. They're recommendations, not daemon guarantees.

A reminder on trust: read methods can surface account emails, local paths, usage, health, and diagnostics; config and account methods change owner-local state; and repair and launch methods can start user-visible provider or Terminal processes. Socket access grants the full local trust described in [Security](../security.md).

## Requests and responses

| Method | Minimal request (after `api_version`) | Success `type` |
| --- | --- | --- |
| `get_server_info` | `{"method":"get_server_info"}` | `server_info` |
| `get_state` | `{"method":"get_state"}` | `state` |
| `get_usage` | `{"method":"get_usage"}` | `usage` |
| `refresh` | `{"method":"refresh"}` | `refresh_started` |
| `get_refresh_job` | `{"method":"get_refresh_job","job_id":"JOB"}` | `refresh_job` |
| `get_provider_health` | `{"method":"get_provider_health"}` | `provider_health` |
| `get_accounts` | `{"method":"get_accounts"}` | `accounts` |
| `get_config` | `{"method":"get_config"}` | `config` |
| `get_pending_notifications` | `{"method":"get_pending_notifications"}` | `pending_notifications` |
| `acknowledge_notifications` | `{"method":"acknowledge_notifications","ids":[1]}` | `notifications_acknowledged` |
| `update_config` | `{"method":"update_config","poll_interval_seconds":300}` | `config` |
| `add_provider_account` | `{"method":"add_provider_account","provider_id":"codex"}` | `add_provider_account` |
| `update_account` | `{"method":"update_account","account_id":"ACCOUNT","hidden":true}` | `account` |
| `remove_account` | `{"method":"remove_account","account_id":"ACCOUNT"}` | `account` |
| `delete_account` | `{"method":"delete_account","account_id":"ACCOUNT"}` | `account_deleted` |
| `get_provider_setup` | `{"method":"get_provider_setup","provider_id":"opencode_go"}` | `provider_setup` |
| `update_provider_setup` | `{"method":"update_provider_setup","provider_id":"opencode_go","settings":{"workspace_id":"wrk_123"}}` | `provider_setup` |
| `repair_provider` | `{"method":"repair_provider","provider_id":"codex"}` | `provider_action` |
| `launch_provider_account` | `{"method":"launch_provider_account","account_id":"ACCOUNT"}` | `provider_action` |

A complete accounts exchange looks like this:

```jsonl
{"api_version":3,"method":"get_accounts"}
{"api_version":3,"type":"accounts","accounts":[]}
```

The exact shapes come from the [schemas](index.md) and [models](models.md).

## Read methods

| Method | What it returns, and in what order | Expected errors | Budget |
| --- | --- | --- | --- |
| `get_server_info` | Current capabilities and providers, in fixed provider order. | Protocol errors only. | 3s |
| `get_state` | The main config/accounts/health/usage/dashboard/forecast view. Storage components come from a single SQLite read transaction; config is read afterward. Lists use the ordering in [models](models.md). | `storage_unavailable` | 3s |
| `get_usage` | The latest visible snapshot per account, plus 30 local-calendar days of dashboard activity and forecasts drawn from at most 35 days / 1,024 observations. Hidden accounts are left out. | `storage_unavailable` | 3s |
| `get_refresh_job` | The current retained job. Jobs live in memory, so unknown or expired IDs fail. | `unknown_refresh_job` | 3s |
| `get_provider_health` | Health for visible supported providers and accounts, ordered by provider then account. | `storage_unavailable` | 3s |
| `get_accounts` | Every supported account — including hidden, disabled, and removed — ordered by provider, profile, then external ID. | `storage_unavailable` | 3s |
| `get_config` | Effective paths, polling, notifications, and visible provider toggles. Credential and profile details are deliberately left out. | `storage_unavailable` | 3s |
| `get_provider_setup` | Safe profile summaries and provider-owned declarative setup fields. Discovery failures can ride along in `discovery_error` with an otherwise successful response. | `unknown_provider`, `internal` | 20s |

Read results reflect storage at the moment each method reads it. They aren't subscriptions, and separate requests don't add up to one consistent snapshot.

## Refresh and notification methods

| Method | Parameters, effects, retry, persistence | Expected errors |
| --- | --- | --- |
| `refresh` | Omitted or `null` `providers` means every enabled collector. A non-empty list is sorted and deduplicated; `[]` is invalid. Starts or joins background work and returns right away. Asking again while overlapping work is active can hand back the same job (`coalesced: true`). Job state isn't persistent, but successful provider data is. | `invalid_argument`, `unknown_provider` |
| `acknowledge_notifications` | `ids` is required; `[]` is a valid no-op. Deletes matching queued rows in one transaction and echoes back every ID you sent, including ones already gone. Idempotent and persistent. | `storage_unavailable` |

See [refresh jobs](refresh-jobs.md) for polling and failure details.

## Configuration and account methods

| Method | Validation and side effects | Idempotency / persistence | Expected errors |
| --- | --- | --- | --- |
| `update_config` | Omitted or `null` fields stay as they are. `providers` is a partial toggle map. `notifications` replaces the whole notification policy. Polling must be at least 60 seconds; the notification rules are in [configuration](../configuration.md). Rebuilds collectors and polling as needed. | Set-like and persistent. Retrying the same complete update is safe. | `invalid_argument` |
| `add_provider_account` | The provider must support `add_account` (Codex, Claude, Grok). A blank or whitespace `display_name` is treated as omitted. Creates and persists an isolated profile, then starts login. The response includes `authentication_url` when the provider CLI exposes its one-time browser link. | Not idempotent; the profile survives a restart. | `unknown_provider`, `unsupported_operation`, `internal` |
| `update_account` | The account must exist. Omitted or `null` `display_name`, `hidden`, or `collection_enabled` leaves that field alone. A blank name also leaves the name alone — v3 can't clear a name to null. | Set-like and persistent. | `unknown_account`, `storage_unavailable`, `internal` |
| `remove_account` | Sets `hidden: true` and `collection_enabled: false` and keeps the history. | Idempotent and persistent. | `unknown_account`, `storage_unavailable`, `internal` |
| `delete_account` | Deletes the database history and tombstones or removes the profile. Irreversible. | Not response-idempotent: retry after success and you get `unknown_account`. | `unknown_account`, `storage_unavailable`, `internal` |
| `update_provider_setup` | Sends provider-owned `settings` string/null values to any provider advertising `setup`. The legacy `workspace_id` field remains accepted for v3 OpenCode clients. Rebuilds collectors. | Set-like and persistent. | `unknown_provider`, `unsupported_operation`, `invalid_argument` |

## External action methods

| Method | Effect | Retry and restart | Expected errors |
| --- | --- | --- | --- |
| `repair_provider` | Validates an optional `account_id`, then opens the provider's login/repair flow. The provider must advertise `repair`. The response includes `authentication_url` when a browser link is available. | Not idempotent — it may open several Terminal or login sessions. The configuration itself persists. | `unknown_provider`, `unknown_account`, `storage_unavailable`, `unsupported_operation`, `internal` |
| `launch_provider_account` | Opens the provider with the account's isolated profile. The provider and account must support launch. | Not idempotent — it may open several sessions. No job persists. | `unknown_account`, `storage_unavailable`, `unsupported_operation` |

These action methods can expose local profile paths to the launched provider process and cause visible Terminal or app activity. Fixture mode rejects sign-in, repair, and launch operations.
