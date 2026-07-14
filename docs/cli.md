# CLI reference

`usage-cli` talks to the running daemon. If you've installed it (or wrapped it) as `usage`, just drop the `cargo run -p usage-cli --` from every example below.

```sh
cargo run -p usage-cli -- [GLOBAL OPTIONS] [COMMAND]
```

Run it with no command and you get `usage` — the dashboard.

## Global options

| Option | Default | What it does |
| --- | --- | --- |
| `--socket-path PATH` | `~/.usagetracker/usage.sock` | Point at a different socket; `USAGE_TRACKER_SOCKET` works too. |
| `--style dashboard\|json` | `dashboard` | Human-friendly display, or one compact JSON value. |
| `--color auto\|always\|never` | `auto` | Dashboard color. `NO_COLOR` is respected. |
| `--max-width N` | `80` | Dashboard width (minimum 60); also `USAGE_TRACKER_MAX_WIDTH`. |

## Commands

| Command | What it does |
| --- | --- |
| `status` | A summary of the daemon, providers, accounts, and how fresh the data is. Alias: `health`. |
| `usage` | Your latest usage. Repeat `-p, --provider ID` or `-a, --account ID` to narrow it; `--all-providers` includes disabled ones; `-d, --details` expands the view. |
| `refresh` | Kick off a refresh and wait for it to finish. Repeat `-p, --provider ID` to limit its scope. |
| `accounts [list]` | List account IDs. Filter or expand with `--provider ID`, `--active`, and `--verbose`. |
| `accounts add PROVIDER [--name NAME]` | Create a fresh, isolated Codex, Claude, or Grok profile and start signing in. |
| `accounts rename ACCOUNT NAME` | Give an account a local display name. |
| `accounts hide\|show ACCOUNT` | Change whether an account appears on the dashboard, without touching collection. |
| `accounts enable\|disable ACCOUNT` | Resume or pause collection. Enabling also makes the account visible again. |
| `accounts remove ACCOUNT` | Pause collection and hide the account; its history stays. |
| `accounts delete ACCOUNT --yes` | Permanently delete the account and its stored usage. |
| `accounts launch ACCOUNT` | Open the provider using this account's managed profile. |
| `providers [list]` | Show which providers are on. |
| `providers enable\|disable PROVIDER` | Turn a provider on or off — takes effect right away and is saved. |
| `providers setup PROVIDER` | Show profiles and provider-owned setup fields/discovery. |
| `providers workspace opencode_go WORKSPACE` | Pick a `wrk_…` workspace; add `--automatic` to go back to discovery. |
| `providers repair PROVIDER [--account ACCOUNT]` | Open the provider's login or repair flow. |
| `config [show]` | Show the effective paths, provider toggles, polling, and notifications. |
| `config set` | Set `--poll-interval SECONDS` and/or `--notifications on\|off`. |

For the exact syntax of any command, run `usage COMMAND --help` — it comes straight from the binary.

## JSON output

CLI JSON is its own thing, separate from the [socket protocol](api/protocol.md). It never wraps results in the socket's `api_version` envelope.

| Command | Top-level JSON |
| --- | --- |
| `status` | `{ "type": "status", ... }` — a CLI-only summary. |
| `usage` | `{ "type": "usage", ... }` — filtered and re-aggregated after your CLI options. |
| `refresh` | `{ "type": "refresh_job", "job": ... }` — once the job finishes. |
| `accounts list` | `{ "type": "accounts", "accounts": [...] }` — after your filters. |
| `providers list`, `config show/set` | `{ "type": "config", "config": ... }`. |
| Mutations | The matching envelope-free API response: `account`, `account_deleted`, `add_provider_account`, `provider_setup`, or `provider_action`. |

Output is a single JSON value plus a `\n` on stdout. Failures print plain text to stderr — `--style json` doesn't produce JSON errors yet. These shapes may change until there's a separate, versioned CLI output format, so don't lean on them too hard.

## Exit codes and scripting

| Code | Meaning |
| --- | --- |
| `0` | The command ran. Note: a completed refresh can still contain failed providers — check `job.provider_results`. |
| `1` | A connection, protocol, daemon, validation, or refresh-job failure. |
| `2` | Bad CLI syntax, or arguments Clap rejected. |

For scripts, ask for JSON explicitly, check the exit code, and look at each provider's refresh status.
