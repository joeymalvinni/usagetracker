# Refresh jobs

`refresh` kicks off provider collection in the background and returns `refresh_started` right away — it doesn't hold the connection open. Poll `get_refresh_job` until `status` is `completed` or `failed`.

```text
queued → running → completed
                 ↘ failed
```

`failed` is only for a job-level failure, like a task panicking. Ordinary provider failures aren't that — they're entries in `provider_results`, and the job still comes back `completed`. So always look through the results.

A discovery failure for an account that has not been saved includes its `profile_id` and a null `account_id`. Preserve that profile ID when offering `request_credential_access`; another account’s successful result does not mean the pending profile recovered. These pending failures do not change the health of existing accounts.

While a job is `running`, `discovered_accounts` grows as provider account
identities are persisted. Each entry contains the provider and account IDs. This
lets an interactive client respond to discovery without polling the global
account list or waiting for slower usage collection. Discovery is progress, not
a successful usage result; keep following the job when the final collection
outcome matters.

## Scope and coalescing

Omitted or `null` scope means every enabled provider. An explicit list is sorted and deduplicated. An active all-provider job covers any narrower request, and an active subset covers a request that fits inside it — either way, you get the existing job back with `coalesced: true`.

Jobs that only partly overlap can have different IDs while still sharing the same in-flight provider call. That's deliberate: it avoids duplicate provider traffic without pretending the two scopes are identical.

## Lifetime

- Jobs live in memory and never survive a daemon restart.
- Active jobs stay queryable.
- The newest 64 completed or failed jobs are kept; older IDs return `unknown_refresh_job`.
- Successful snapshots, health, daily usage, and backoff are persisted independently of job retention.
- Disconnecting the client that started a job doesn't cancel it.

Polling every 250–500 ms is plenty for completion-only clients. An interactive
client may poll a manually started, provider-scoped job more frequently while it
waits for discovery progress. Give yourself an overall wait budget that suits
real provider collection — the CLI uses two minutes, the menu app five — and
start a fresh refresh if a restart makes the job unknown.
