# Data and privacy

UsageTracker has no backend. Everything stays on your Mac, except for the requests it makes directly to the providers you've configured.

## What's stored

| Where | What's in it |
| --- | --- |
| `config.json` | Provider toggles, profile paths and labels, your notification policy, workspace selection, and any manual cookie headers. |
| `usage.sqlite3` | Provider and account IDs, your email when a provider reports it, normalized snapshots and diagnostics, daily usage and cost, health, backoff, and notification state. |
| `usage-daemon.log*` | Operational messages and sanitized failure details. |
| `ui/config.json` | Menu presentation preferences. |
| Keychain | Provider credentials, or filtered cached cookie headers, wherever a provider integration uses them. |

Raw provider response bodies are parsed in memory and never written to disk. Local provider logs and databases are read from their known or configured paths — UsageTracker doesn't copy their full contents into its own storage.

## How long it's kept

- **Normalized snapshots:** 90 days, and at most 10,000 per account.
- **Daily usage and cost:** kept until you permanently delete the account.
- **Provider health and backoff:** just the latest state per provider/account.
- **Pending notifications:** the newest 1,000 are stored; a read returns the oldest 100.
- **Refresh jobs:** in memory only — the newest 64 completed jobs, cleared when the daemon restarts.
- **App daemon logs:** a 5 MiB active file plus three rotated archives.

## Deleting things

Here's what the different actions actually do:

- **Hide** changes the display only.
- **Disable** pauses collection.
- **Remove** does both, and keeps your history.
- **Permanent deletion** removes the account's snapshots, daily usage, health, and notification state, drops its profile access, and leaves a tombstone so the profile isn't silently recreated.

To wipe everything, quit the app and daemon and delete `~/.usagetracker/`. That won't touch your actual provider accounts, any history the provider keeps, your browser cookies, or provider credential files — and Keychain items owned by the providers aren't removed either.

One more thing: a legacy schema upgrade may reset reproducible local data. When that happens, the daemon logs it — and it will never erase an unrelated SQLite database it doesn't recognize.
