# CLI interface specification

Status: proposed

Scope: `usage` command-line interface

Compatibility target: existing daemon socket API v3

## Summary

The CLI uses provider names as top-level commands for the most common task: checking one provider.

```text
usage                      Full dashboard for all enabled providers
usage codex                Full dashboard filtered to Codex
usage claude               Full dashboard filtered to Claude
usage grok                 Full dashboard filtered to Grok
usage opencode             Full dashboard filtered to OpenCode Go
usage summary              Compact one-line-per-provider rollup
usage activity             Daily token and cost timeline
usage status               Daemon and collection health
usage refresh [PROVIDER]   Collect fresh data and wait for completion
usage accounts ...         Manage accounts
usage providers ...        Configure providers
usage config ...           Configure the daemon
```

Provider commands are a convenience form of the existing provider-filtered dashboard. Administrative command groups remain unchanged.

## Goals

- Make `usage codex` and `usage claude` do the obvious thing.
- Keep `usage` useful without requiring a subcommand.
- Give summary, historical activity, and operational health distinct views.
- Preserve every existing account, provider, refresh, and configuration operation.
- Preserve the existing global output, color, socket, and width controls.
- Keep human output readable while retaining machine-readable JSON.
- Support provider IDs added by a newer daemon without requiring a hard-coded Clap variant for each one.

## Non-goals

- Changing provider collection behavior or normalized usage models.
- Changing the daemon's polling schedule.
- Replacing account IDs with potentially ambiguous display names in mutating commands.
- Treating local token estimates as provider billing records.
- Exposing a history of individual daemon polls in the first implementation. The current API exposes daily activity and the latest snapshot, but it cannot list historical refresh jobs or observations. See [Activity versus poll history](#activity-versus-poll-history).

## Command grammar

```text
usage [GLOBAL OPTIONS]
usage [GLOBAL OPTIONS] <PROVIDER> [DASHBOARD OPTIONS]
usage [GLOBAL OPTIONS] summary [PROVIDER...] [SUMMARY OPTIONS]
usage [GLOBAL OPTIONS] activity [PROVIDER...] [ACTIVITY OPTIONS]
usage [GLOBAL OPTIONS] status [PROVIDER...] [STATUS OPTIONS]
usage [GLOBAL OPTIONS] refresh [PROVIDER...] [REFRESH OPTIONS]
usage [GLOBAL OPTIONS] accounts [ACCOUNT COMMAND]
usage [GLOBAL OPTIONS] providers [PROVIDER COMMAND]
usage [GLOBAL OPTIONS] config [CONFIG COMMAND]
```

Global options are accepted before or after a subcommand.

### Reserved top-level words

These tokens are commands and always win over provider-name resolution:

```text
summary
activity
status
health
refresh
accounts
providers
config
usage
help
version
```

If a future provider uses a reserved word as its ID, its dashboard remains reachable through the unambiguous form:

```sh
usage usage --provider status
```

The `usage usage` subcommand is therefore not merely a compatibility shim: it is the permanent, unambiguous spelling of the provider-filtered dashboard and is never removed. It may be hidden from top-level help, but it must continue to parse in every release.

## Global options

The existing global options remain supported:

| Option | Default | Behavior |
| --- | --- | --- |
| `--socket-path PATH` | `~/.usagetracker/usage.sock` | Connect to another daemon socket. `USAGE_TRACKER_SOCKET` is also supported. |
| `--json` | off | Emit machine-readable JSON instead of the human dashboard. Disables color and ignores `--max-width`. JSON is always one value followed by one newline. |
| `--style dashboard\|json` | `dashboard` | Legacy spelling of the output selector. Hidden from help; `--style json` remains equivalent to `--json`. |
| `--color auto\|always\|never` | `auto` | Control ANSI color in human output. `NO_COLOR` is respected. |
| `--max-width N` | `80` | Limit human output width. The minimum remains 60 columns. `USAGE_TRACKER_MAX_WIDTH` is also supported. |
| `-h, --help` | — | Show help for the selected command. |
| `-V, --version` | — | Show the CLI version. |

`--json` is the preferred spelling. `--style` is retained as a hidden legacy option so existing scripts keep working; the two must never disagree, and passing both with conflicting values is a parse error (exit `2`).

## Provider-name resolution

### Parsing algorithm

1. Clap attempts to match the first non-option token against a reserved top-level command.
2. If no built-in command matches, the token and its remaining arguments are captured as an external subcommand.
3. The token is normalized through the provider alias table.
4. The CLI validates the normalized ID against the daemon's provider descriptors and configured provider IDs.
5. A valid provider runs the provider-filtered dashboard.
6. An invalid provider exits with a syntax error and suggests the closest provider or command when there is a clear match.

The fallback must not silently ignore an unknown token or render an empty dashboard.

Example:

```text
$ usage claud
error: unknown command or provider 'claud'

  tip: a similar provider exists: 'claude'
  usage claude
```

### Provider aliases

The CLI accepts friendly spellings at its boundary while continuing to use canonical IDs in API requests and JSON:

| Input | Canonical ID | Display name |
| --- | --- | --- |
| `codex` | `codex` | Codex |
| `claude` | `claude` | Claude |
| `grok` | `grok` | Grok |
| `xai` | `grok` | Grok |
| `opencode` | `opencode_go` | OpenCode Go |
| `opencode-go` | `opencode_go` | OpenCode Go |
| `opencode_go` | `opencode_go` | OpenCode Go |

Aliases affect command parsing only. Configuration files, socket messages, and JSON output use canonical IDs.

Provider matching is ASCII case-insensitive for the built-in aliases. IDs discovered only from the daemon are matched exactly after alias resolution.

Daemon-reported IDs take precedence over built-in aliases: if the daemon ever reports a provider whose canonical ID collides with an alias (for example a real `xai` provider distinct from `grok`), the token resolves to the daemon's provider and the built-in alias is ignored for that token.

## Dashboard commands

### `usage`

Shows the complete dashboard across all enabled providers and visible accounts.

```sh
usage
usage --details
usage --account ACCOUNT
usage --all-providers
```

Options:

| Option | Behavior |
| --- | --- |
| `-a, --account ACCOUNT` | Include only this stable account ID. Repeatable. |
| `--all-providers` | Include stored data for disabled providers. |
| `-d, --details` | Show extra windows, credits, forecasts, provenance-sensitive metadata, and identity. |

The default view retains the existing dashboard structure:

1. Aggregate overview.
2. Seven-day activity chart.
3. One panel per visible provider account.

The overview contains lifetime tokens when available, tracked spend, peak daily tokens, and current and longest streaks. Values with mixed account-wide and local scope must retain the existing provenance warning; they must not be presented as a provider bill.

Provider panels contain, when available:

- provider, collection mode, plan, and account identity;
- session, weekly, and monthly quota windows;
- percent remaining and reset time;
- observed tokens and estimated cost;
- pace against the selected quota window;
- latest collection time;
- with `--details`, extra windows, credits, reset credits, forecast, identity, and additional diagnostics already approved for CLI display.

Hidden accounts are excluded. Disabled providers are excluded unless explicitly selected or `--all-providers` is present.

### `usage <PROVIDER>`

Shows a focused dashboard for one provider.

```sh
usage codex
usage claude --details
usage grok --account grok:default
usage opencode --json
```

Options are the dashboard options listed for `usage`, except `--all-providers` is unnecessary. Naming a provider is an explicit selection, so stored data for that provider is shown even if collection is disabled. Human output displays a `collection disabled` notice in that case.

Human output contains only the provider-named seven-day activity chart (for example, `Codex Activity · last 7 days`) followed by the visible account panels for that provider. It omits the aggregate overview and mixed-scope totals warning because those belong to the multi-provider dashboard. JSON retains the complete filtered aggregate response.

The legacy `usage usage --provider codex` form carries the same intent and therefore gets the same explicit-selection semantics: it also shows stored data for a disabled provider. `usage codex` and `usage usage --provider codex` must always produce identical output.

When a provider has multiple visible accounts, each account gets its own provider panel. `--account` accepts stable account IDs and is useful for narrowing this view.

If the provider is supported but has no snapshot, the command succeeds and renders an actionable empty state:

```text
Claude

No usage has been collected yet.
State       credentials missing
Next step   usage providers repair claude
```

The suggested action depends on health and configuration:

| State | Suggested action |
| --- | --- |
| Disabled | `usage providers enable PROVIDER` |
| No account and add-account is supported | `usage accounts add PROVIDER` |
| Missing or invalid credentials | `usage providers repair PROVIDER` |
| Rate limited or backing off | Show the retry/reset information; do not suggest repair. |
| Provider unavailable | Show the sanitized provider error. |

If an explicit `--account` does not belong to the selected provider, the command fails instead of displaying an empty dashboard.

### Dashboard JSON

`usage`, `usage <PROVIDER>`, and the legacy `usage usage` form retain the current envelope-free `usage` response:

```json
{
  "type": "usage",
  "snapshots": [],
  "dashboard": {
    "accounts": [],
    "days": [],
    "pricing": {
      "priced_tokens": 0,
      "unpriced_tokens": 0,
      "covered_percent": 0.0,
      "unpriced_models": []
    },
    "provenance": {
      "scopes": [],
      "qualities": [],
      "partial": false,
      "estimated": false,
      "mixed_scope": false,
      "explanation": "Activity reflects data observed on this Mac or in the configured local roots."
    }
  }
}
```

The arrays and aggregate dashboard are filtered to the selected provider and accounts. `forecasts` and `window_provenance` retain their existing shapes and are omitted when empty. Filtering must rebuild aggregate totals and provenance from the selected account summaries; it must not merely remove snapshots while leaving global totals unchanged.

## `usage summary`

Shows a compact one-line-per-provider rollup intended for quick scanning.

```sh
usage summary
usage summary codex
usage summary codex claude
usage summary --all-providers
usage summary --json
```

Syntax:

```text
usage summary [PROVIDER...]
```

Providers are selected positionally, matching `usage refresh`. There is no `--provider` option on this new command; positionals are the only spelling, so there is exactly one way to write a selection. Positional provider names also work as reserved-word escape hatches (`usage summary status` selects a hypothetical provider named `status`).

Options:

| Option | Behavior |
| --- | --- |
| `-a, --account ACCOUNT` | Include a stable account ID. Repeatable. |
| `--all-providers` | Include disabled providers with stored data or configured state. |

Human output uses one physical row per provider at normal terminal widths:

```text
Provider     Accounts  Limits                        Today    30d    Cost  Updated
Codex               2  5h 18–82% left · wk 61–94%     128K   2.8M   $8.41  2m ago
Claude              1  5h 74% left · wk 42%            91K   1.4M   $5.72  4m ago
OpenCode Go         1  month 88% left                  12K    84K   $0.39  8m ago
Grok                1  credits 340                       —      —       —  1h ago (stale)
```

Column semantics:

| Column | Meaning |
| --- | --- |
| `Provider` | Human display name. |
| `Accounts` | Count of selected visible accounts represented by the row. |
| `Limits` | Important session, weekly, monthly, or credit windows. |
| `Today` | Aggregate observed tokens for the current local calendar day. |
| `30d` | Aggregate observed tokens in the returned 30-day lookback. |
| `Cost` | Aggregate tracked or estimated cost for that lookback. `—` means unavailable, not zero. |
| `Updated` | Age of the least-fresh selected account snapshot, so one stale account cannot be hidden by a newer one. |

Quota percentages are **remaining**, and every human rendering must say so explicitly (`74% left`), because a bare `74%` is ambiguous between used and remaining. Whatever direction the full dashboard and menu bar use, all surfaces must agree; if any existing surface shows percent *used*, reconcile that before shipping summary rather than shipping two conventions.

Multi-account quota percentages are never averaged. When accounts expose comparable windows, the cell shows the minimum-to-maximum remaining range, such as `18–82% left`. A single value is shown when only one comparable window exists or all values are equal. Reset times may be appended for a single-account row but are omitted for differing multi-account resets.

A row whose least-fresh snapshot is older than twice the configured poll interval (or older than 30 minutes, whichever is greater) is marked `(stale)` in `Updated`, because its `Today` and `30d` aggregates may silently under-count. The same condition sets `stale: true` in JSON.

When width is constrained, columns disappear in this order: `Accounts`, `Cost`, `Today`. `Provider`, `Limits`, `30d`, and `Updated` remain. Values are never silently truncated into a misleading number.

Disabled providers are omitted by default. With `--all-providers`, they appear with a `disabled` marker. A configured provider without data appears with `no data` rather than disappearing.

### Summary JSON

JSON is explicit and does not contain the human-formatted range strings:

```json
{
  "type": "summary",
  "generated_at": "2026-07-13T19:00:00Z",
  "providers": [
    {
      "provider_id": "codex",
      "display_name": "Codex",
      "enabled": true,
      "data_state": "available",
      "account_count": 2,
      "limits": [
        {
          "role": "session",
          "label": "5h",
          "minimum_percent_remaining": 18.0,
          "maximum_percent_remaining": 82.0,
          "minimum_remaining": null,
          "maximum_remaining": null,
          "unit": null,
          "next_reset_at": "2026-07-13T21:00:00Z",
          "account_count": 2
        }
      ],
      "today_tokens": 128000,
      "lookback_tokens": 2800000,
      "lookback_cost_usd": 8.41,
      "oldest_snapshot_at": "2026-07-13T18:58:00Z",
      "stale": false
    }
  ]
}
```

`data_state` is `available`, `no_data`, or `disabled`. A limit's `role` is `session`, `weekly`, `monthly`, `credits`, or `other`. Percentage-based limits use the percent fields. Absolute limits such as credits use `minimum_remaining`, `maximum_remaining`, and `unit`; their percent fields are `null`. `next_reset_at` is the earliest known reset in the group, or `null`.

Nullable or unavailable values are serialized as `null`. Token counts remain integers and currency remains an unformatted number. Canonical provider IDs are used.

## `usage activity`

Shows daily token and cost activity over a bounded recent period.

```sh
usage activity
usage activity codex
usage activity codex claude
usage activity claude --days 7
usage activity --account ACCOUNT --days 30
```

Syntax:

```text
usage activity [PROVIDER...]
```

Providers are selected positionally, matching `usage refresh` and `usage summary`; there is no `--provider` option on this new command.

Options:

| Option | Default | Behavior |
| --- | --- | --- |
| `-a, --account ACCOUNT` | all visible | Include a stable account ID. Repeatable. |
| `--days N` | `14` | Show 1 through 30 local-calendar days. |
| `--all-providers` | off | Include activity retained for disabled providers. |

The 30-day maximum exists because the daemon's `get_usage` response carries a 30-day daily-activity lookback; the CLI cannot render days it never receives. `--days 0` or `--days 31` is rejected at parse time with exit code `2`. If a future daemon returns a longer lookback, the parse-time limit is raised in step with the API — out-of-range values always error rather than silently clamping.

Human output is chronological, oldest to newest:

```text
Activity · Jul 7–13 · Codex

Date         Tokens        Cost  Coverage
Mon Jul 07        0           —         —
Tue Jul 08     84.2K       $0.31      100%
Wed Jul 09    126.8K       $0.49       92%
Thu Jul 10     91.4K       $0.37      100%
Fri Jul 11    202.1K       $0.83       87%
Sat Jul 12     31.7K       $0.14      100%
Sun Jul 13    128.0K       $0.52       96%

Total          664.2K      $2.66       94%
Scope          this Mac · estimated · partial
```

Rules:

- Calendar boundaries use the user's local timezone.
- Every date in the selected range is printed. A missing date has zero observed tokens.
- Cost is `—` when cost data is unavailable. It is `$0.00` only when the data explicitly represents a known zero.
- Coverage is priced tokens divided by priced plus unpriced tokens. It is not the share of quota remaining.
- Mixed-scope selections show the existing mixed-scope explanation below the table.
- Selecting a provider explicitly includes its retained activity even when the provider is disabled.
- Filters rebuild daily totals, pricing coverage, and provenance from the selected accounts.

### Activity JSON

```json
{
  "type": "activity",
  "range": {
    "days": 7,
    "start_date": "2026-07-07",
    "end_date": "2026-07-13",
    "timezone": "America/Los_Angeles"
  },
  "filters": {
    "providers": ["codex"],
    "accounts": []
  },
  "days": [
    {
      "date": "2026-07-07",
      "tokens": 0,
      "cost_usd": null,
      "priced_tokens": 0,
      "unpriced_tokens": 0
    }
  ],
  "pricing": {
    "priced_tokens": 0,
    "unpriced_tokens": 0,
    "covered_percent": 0.0,
    "unpriced_models": []
  },
  "provenance": {
    "scopes": [],
    "qualities": [],
    "partial": false,
    "estimated": false,
    "mixed_scope": false,
    "explanation": "Activity reflects data observed on this Mac or in the configured local roots."
  }
}
```

The `days` array always contains exactly the requested number of calendar days, including zero-filled days. `pricing` and `provenance` use the shared core model shapes.

### Activity versus poll history

For this interface, **activity** means provider or locally observed daily token and cost activity. It does not mean a log of every daemon polling attempt.

The current socket API supports:

- the latest visible usage snapshot;
- 30 days of daily activity;
- current provider health;
- a refresh job when its ID is already known.

It does not support listing historical refresh jobs or raw usage-window observations. A future poll-history mode would require a bounded API such as `list_refresh_jobs` or `get_usage_observations` before adding a command like:

```sh
usage activity --polls
```

That future mode must remain distinct from daily token activity because a successful poll can record no new consumption, and a day with consumption is not necessarily a poll event.

## `usage status`

Shows operational health rather than consumption.

```sh
usage status
usage status codex
usage health
usage status --json
```

Syntax:

```text
usage status [PROVIDER...]
```

Providers are selected positionally, matching the other read commands; there is no `--provider` option.

Options:

| Option | Behavior |
| --- | --- |
| `-a, --account ACCOUNT` | Include a stable account ID. Repeatable. |
| `--all-providers` | Include disabled providers. |

`health` remains an alias for `status`.

Human output retains the existing daemon header and provider/account table:

```text
Usage Tracker

Daemon      connected
Socket      ~/.usagetracker/usage.sock
Poll        every 300s
Providers   4 enabled
Updated     Jul 13, 12:00 PM

Provider  Identity          Plan  State         Usage  Updated  Detail
Codex     joey@example.com  Plus  ok            fresh  2m ago   app server
Claude    Work              Team  rate_limited  stale  1h ago   retry after 12:40 PM
```

Status values retain the daemon model: `ok`, `credentials_missing`, `auth_failed`, `rate_limited`, `provider_error`, `parse_error`, `backing_off`, and `disabled`.

An unhealthy provider does not make the status command itself fail. The command successfully reported daemon state, so its exit code is `0`. Connection, protocol, and validation errors still fail.

### Status JSON

JSON retains the existing CLI-only status shape and applies the selected filters:

```json
{
  "type": "status",
  "daemon": "connected",
  "socket_path": "/Users/me/.usagetracker/usage.sock",
  "poll_interval_seconds": 300,
  "enabled_provider_count": 4,
  "updated_at": "2026-07-13T19:00:00Z",
  "providers": []
}
```

## `usage refresh`

Starts a manual refresh, waits for its terminal state, and prints one result per provider account.

```sh
usage refresh
usage refresh codex
usage refresh codex claude
usage refresh --provider codex
```

Syntax:

```text
usage refresh [PROVIDER...]
```

Options:

| Option | Behavior |
| --- | --- |
| `-p, --provider PROVIDER` | Compatibility form; repeatable. Mutually exclusive with positional providers. |

No provider means all enabled providers. Provider aliases are normalized before the request. Duplicate providers are removed while preserving the user's first-seen order for validation; rendered results follow the daemon's deterministic order.

The command retains the current two-minute client wait timeout and 250 ms job polling interval. A coalesced refresh is followed exactly like a newly created refresh.

Human output retains the current refresh result table with start, finish, duration, provider, identity, plan, status, mode, collection time, and sanitized message.

A completed job may contain provider failures. This remains exit code `0`; scripts must inspect `provider_results`. A job-level failure, timeout, connection failure, or protocol failure exits `1`.

JSON retains the existing shape:

```json
{
  "type": "refresh_job",
  "job": {}
}
```

## Account administration

The `accounts` namespace and its behavior remain unchanged.

```text
usage accounts [list]
usage accounts add PROVIDER [--name NAME]
usage accounts rename ACCOUNT NAME
usage accounts hide ACCOUNT
usage accounts show ACCOUNT
usage accounts enable ACCOUNT
usage accounts disable ACCOUNT
usage accounts remove ACCOUNT
usage accounts delete ACCOUNT --yes
usage accounts launch ACCOUNT
```

### `usage accounts [list]`

Lists accounts and their stable IDs.

Options:

| Option | Behavior |
| --- | --- |
| `-p, --provider PROVIDER` | Include only the selected provider. Provider aliases are accepted. |
| `--active` | Hide removed, hidden, and collection-disabled accounts. |
| `-v, --verbose` | Add profile and external-account ID columns. |

With `--json`, the result remains:

```json
{"type":"accounts","accounts":[]}
```

### Account mutations

| Command | Behavior |
| --- | --- |
| `accounts add PROVIDER [--name NAME]` | Create or resume an isolated managed provider profile and start sign-in. |
| `accounts rename ACCOUNT NAME` | Set a local display name. |
| `accounts hide ACCOUNT` | Hide the account from normal views without stopping collection. |
| `accounts show ACCOUNT` | Make a hidden account visible. |
| `accounts enable ACCOUNT` | Resume collection and make the account visible. |
| `accounts disable ACCOUNT` | Pause collection while keeping history visible. |
| `accounts remove ACCOUNT` | Pause collection and hide the account while retaining history. |
| `accounts delete ACCOUNT --yes` | Permanently delete the account and stored usage. |
| `accounts launch ACCOUNT` | Launch the provider with the account's isolated profile. |

Permanent deletion continues to require `--yes`. Without it, the command exits `1` and suggests `accounts remove` as the history-preserving alternative.

Mutating commands continue to print the matching envelope-free API response in JSON mode.

## Provider administration

The `providers` namespace and its behavior remain unchanged.

```text
usage providers [list]
usage providers enable PROVIDER
usage providers disable PROVIDER
usage providers setup PROVIDER
usage providers workspace PROVIDER [WORKSPACE | --automatic]
usage providers repair PROVIDER [--account ACCOUNT]
```

Provider aliases are accepted for every `PROVIDER` argument and normalized before API requests.

| Command | Behavior |
| --- | --- |
| `providers [list]` | Show provider enablement from effective configuration. |
| `providers enable PROVIDER` | Enable collection immediately and persist configuration. |
| `providers disable PROVIDER` | Disable collection immediately and persist configuration. |
| `providers setup PROVIDER` | Show discovered profiles and provider-owned setup fields. |
| `providers workspace PROVIDER WORKSPACE` | Select a provider workspace. |
| `providers workspace PROVIDER --automatic` | Clear the explicit workspace and return to discovery. |
| `providers repair PROVIDER [--account ACCOUNT]` | Launch provider authentication or repair. |

## Daemon configuration

The `config` namespace remains unchanged.

```text
usage config [show]
usage config set [--poll-interval SECONDS] [--notifications on|off]
```

`config set` requires at least one setting. Updates take effect live and are persisted by the daemon.

Human output shows effective paths, polling interval, notification state, and provider toggles. JSON retains the envelope-free config response:

```json
{"type":"config","config":{}}
```

## Empty states

Read commands distinguish these cases:

| Situation | Behavior |
| --- | --- |
| No daemon connection | Exit `1`; show socket path and how to start the daemon. |
| No providers enabled | Dashboard and summary succeed with instructions to run `usage providers enable PROVIDER`. |
| Provider supported but no account | Provider dashboard succeeds with an add-account or repair suggestion. |
| Account exists but no snapshot | Show account identity, current health, and `no usage collected yet`. |
| Filters select no known account | Exit `1` and name the unknown or mismatched account. |
| Selected range contains no activity | Activity succeeds and renders zero days plus `No observed activity in this period.` |
| Cost unavailable | Render `—`; JSON uses `null`. |

Empty data is not automatically an error when the user's selector is valid.

## Errors and exit codes

| Code | Meaning |
| --- | --- |
| `0` | The command completed, including valid empty views and completed refresh jobs containing provider-level failures. |
| `1` | Connection, daemon, protocol, validation, job-level refresh, timeout, or action failure. |
| `2` | Invalid command syntax or arguments rejected during CLI parsing. |

Errors go to stderr. Normal human or JSON results go to stdout.

`--json` continues to produce plain-text errors on stderr in this version. Versioned structured CLI errors are a separate design decision.

Unknown external subcommands are treated as syntax errors and exit `2`. A name that resolves to a provider but is rejected by the daemon exits `1` with the daemon's sanitized validation error.

## Help output

Top-level help emphasizes daily commands before administration:

```text
Inspect and manage UsageTracker

Usage: usage [OPTIONS] [COMMAND]

Commands:
  <provider>   Show one provider, for example `usage codex`
  summary      Show a compact provider rollup
  activity     Show recent token and cost activity
  status       Show daemon and provider health
  refresh      Poll providers immediately
  accounts     List and manage provider accounts
  providers    Inspect and configure providers
  config       Inspect and edit daemon configuration
  help         Print help for a command

Examples:
  usage
  usage codex
  usage claude --details
  usage summary
  usage activity codex --days 7
  usage refresh codex
```

Because dynamic external subcommands cannot be enumerated reliably by Clap, the help text explicitly names the provider shortcut and examples. If provider descriptors can be fetched cheaply and safely, shell completion may enumerate them; help must not require a running daemon.

## Backward compatibility

Existing commands remain valid. The `usage usage` form is permanent (it is the reserved-word escape hatch); the other legacy spellings remain for at least one compatibility release:

| Existing form | Preferred form |
| --- | --- |
| `usage` | `usage` |
| `usage usage` | `usage` |
| `usage usage --provider codex` | `usage codex` |
| `usage usage --provider codex --details` | `usage codex --details` |
| `usage refresh --provider codex` | `usage refresh codex` |
| `usage --style json` | `usage --json` |
| `usage status` | `usage status` |
| `usage health` | `usage status` |
| `usage accounts ...` | unchanged |
| `usage providers ...` | unchanged |
| `usage config ...` | unchanged |

The `usage usage` subcommand, `refresh --provider` option, and `--style` option may be hidden from top-level help, but must not emit deprecation warnings in JSON mode because warnings would make otherwise clean script output harder to consume. If human-mode deprecation warnings are added later, they go to stderr.

The existing JSON shapes for dashboard, status, refresh, accounts, provider actions, and configuration remain unchanged. `summary` and `activity` introduce new CLI-only JSON shapes. CLI JSON remains separate from the versioned socket envelope.

## Implementation notes

### Clap routing

The intended implementation uses a final `#[command(external_subcommand)]` command variant. Built-in variants are declared normally, so reserved commands take precedence. The captured token is then parsed as a provider dashboard invocation, including dashboard options such as `--details` and `--account`.

If external-subcommand option parsing proves too awkward, an equivalent two-pass parser is acceptable:

1. parse global options and known commands;
2. on an unknown first token, validate it as a provider and parse the remaining dashboard options.

The observable behavior in this document takes precedence over the internal parser technique.

### Data requirements

No daemon API change is required for the initial command set:

| View | Existing request/data |
| --- | --- |
| Full dashboard | `get_usage`, `get_accounts`, `get_config` |
| Provider dashboard empty-state health | `get_state` or `get_provider_health` plus existing usage/account/config requests |
| Summary | Filtered `get_usage`, accounts, and config |
| Activity | `UsageDashboardSummary.days` and per-account summaries from `get_usage` |
| Status | `get_state` |
| Refresh | `refresh`, followed by `get_refresh_job` |

Summary and activity are CLI renderings over existing typed data, not new provider collection modes.

### Filtering order

All read views apply selectors in this order:

1. Start with server-returned visible snapshots and account summaries.
2. Apply default provider enablement unless selection is explicit or `--all-providers` is set.
3. Apply provider selectors.
4. Apply account selectors and validate provider/account compatibility.
5. Remove forecasts and provenance rows that no longer correspond to selected snapshots.
6. Rebuild dashboard daily totals, pricing coverage, and aggregate provenance from selected account summaries.

This order prevents totals, forecasts, or provenance from leaking in from filtered-out accounts.

## Acceptance criteria

- `usage codex` renders the same provider data as `usage usage --provider codex`; both are explicit selections that include a disabled provider's stored data.
- `usage claude --details` applies `--details` to the Claude-only dashboard.
- `usage opencode`, `usage opencode-go`, and `usage opencode_go` send `opencode_go` to the daemon.
- Built-in commands such as `usage status` are never interpreted as provider IDs.
- An unknown token produces a useful error rather than an empty dashboard.
- `usage` continues to render the full dashboard.
- `usage summary` emits no more than one human-output row per provider.
- `usage summary codex claude` and `usage activity codex claude` accept multiple positional providers; the new read commands reject `--provider`.
- Multi-account percentages in summary are ranged, never averaged, and human output labels percentages as remaining (`% left`).
- A summary row whose least-fresh snapshot exceeds the staleness threshold is marked `(stale)` in human output and `stale: true` in JSON.
- `usage activity --days N` returns exactly `N` local-calendar days for `1 <= N <= 30`.
- Filtering summary or activity recomputes totals and provenance.
- Explicit provider selection can show retained data for a disabled provider.
- Hidden accounts remain excluded from normal read views.
- Provider aliases are accepted by admin commands: `usage providers enable opencode-go` sends `opencode_go` to the daemon.
- `usage <PROVIDER> --json` and legacy `usage usage --provider PROVIDER --style json` produce byte-identical output.
- All existing account, provider, config, status, refresh, and JSON workflows remain available.
- Human output respects color and maximum-width settings.
- JSON output contains no ANSI escapes and ends with exactly one newline.
- Existing exit-code behavior is preserved.
