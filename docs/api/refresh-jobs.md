# Refresh jobs

`refresh` kicks off provider collection in the background and returns `refresh_started` right away — it doesn't hold the connection open. Poll `get_refresh_job` until `status` is `completed` or `failed`.

```text
queued → running → completed
                 ↘ failed
```

`failed` is only for a job-level failure, like a task panicking. Ordinary provider failures aren't that — they're entries in `provider_results`, and the job still comes back `completed`. So always look through the results.

## Scope and coalescing

Omitted or `null` scope means every enabled provider. An explicit list is sorted and deduplicated. An active all-provider job covers any narrower request, and an active subset covers a request that fits inside it — either way, you get the existing job back with `coalesced: true`.

Jobs that only partly overlap can have different IDs while still sharing the same in-flight provider call. That's deliberate: it avoids duplicate provider traffic without pretending the two scopes are identical.

## Lifetime

- Jobs live in memory and never survive a daemon restart.
- Active jobs stay queryable.
- The newest 64 completed or failed jobs are kept; older IDs return `unknown_refresh_job`.
- Successful snapshots, health, daily usage, and backoff are persisted independently of job retention.
- Disconnecting the client that started a job doesn't cancel it.

Polling every 250–500 ms is plenty for first-party clients. Give yourself an overall wait budget that suits real provider collection — the CLI uses two minutes, the menu app five — and start a fresh refresh if a restart makes the job unknown.
