# usagetracker

local ai usage reports


```
╭─ Overview ───────────────────────────────────────────────────────────────────╮
│ Lifetime tokens  2.9B       Peak tokens    185.9M                            │
│ Tracked spend    $1720.31   Current streak 18 days                           │
│ Longest streak   18 days                                                     │
╰──────────────────────────────────────────────────────────────────────────────╯

╭─ Activity · last 7 days ─────────────────────────────────────────────────────╮
│ Sat    6.1M  █░░░░░░░░░░░░░░░░░░░░░░░░░░░                                    │
│ Sun     14M  ██░░░░░░░░░░░░░░░░░░░░░░░░░░                                    │
│ Mon    5.9M  █░░░░░░░░░░░░░░░░░░░░░░░░░░░                                    │
│ Tue   55.3M  ████████░░░░░░░░░░░░░░░░░░░░                                    │
│ Wed   48.4M  ███████░░░░░░░░░░░░░░░░░░░░░                                    │
│ Thu  185.9M  ████████████████████████████                                    │
│ Fri  141.4M  █████████████████████░░░░░░░                                    │
╰──────────────────────────────────────────────────────────────────────────────╯

╭─ Claude · web · Team ───────────────────────────────────── alex@example.com ─╮
│ Session   █████████████████████░░░░░░░   75%  ·  resets in 4h 14m            │
│ Weekly    ████████████████████████░░░░   84%  ·  resets in 4d 2h             │
│ Usage     11.1M today · 98.2M 30d                                            │
│ Pace      under · 16% used vs 41% expected                                   │
│ Updated   just now                                                           │
╰──────────────────────────────────────────────────────────────────────────────╯

╭─ Codex · app-server · Team ─────────────────────────────── alex@example.com ─╮
│ Session   ████████████████████████████   99%  ·  resets in 4h 57m            │
│ Weekly    ████████████████████████████  100%  ·  resets in 6d 18h            │
│ Usage     152.1K today · 2.4M 30d · 937.6M lifetime                          │
│ Pace      on track · 0% used vs 3% expected                                  │
│ Updated   2m ago                                                             │
╰──────────────────────────────────────────────────────────────────────────────╯

Pass `--details` to add per-window sub-limits, credits, forecast, and identity to
each panel; the box width follows your terminal.
```

## Usage

With [`just`](https://just.systems/) installed, the common development commands are:

```sh
just build               # Build Rust and the development macOS app bundle
just app                 # Build and launch the development macOS app bundle
just app-dev             # Explicit development build and launch
just app-release         # Optimized release build and launch
just daemon              # Run the daemon in the foreground
just cli status          # Run any CLI command
just test                # Run all Rust tests
just check               # Run CI-equivalent Rust, Swift, and audit checks
just                     # List every available recipe
```

Arguments after `just daemon` or `just cli` are passed through to the underlying binary. The existing Cargo and Swift commands below remain available directly.

Build everything:

```sh
cargo build
```

Run the daemon in one terminal:

```sh
cargo run -p usage-daemon
```

The daemon starts in the foreground, creates missing local files, opens a Unix socket, runs an initial refresh, and then refreshes enabled providers on the configured polling interval. By default it uses:

- `~/.usagetracker/config.json`
- `~/.usagetracker/usage.sqlite3`
- `~/.usagetracker/usage.sock`

The Swift menu bar app uses the same daemon socket by default and stores UI-only preferences under:

- `~/.usagetracker/ui/config.json`

The current SQLite schema is intentionally disposable because provider usage can be recollected. A
positively identified pre-v2 UsageTracker database is reset on upgrade; this also discards local-only
account names, hidden/removal state, and collection preferences. The daemon logs that reset explicitly.
If a non-empty database cannot be positively identified as UsageTracker data, startup refuses to modify
it and asks for an empty `--db-path` instead.

## Development fixtures

Launch the real app and bundled daemon against a reset synthetic SQLite database:

```sh
just fixture                 # full dashboard, accounts, activity, costs, forecasts, and an error
just fixture notifications   # low/exhausted windows plus queued desktop alerts
```

Fixture launches store every file under `.dev/fixture/`, including UI preferences, config, database,
socket, and daemon log. They never read or replace `~/.usagetracker`. Each launch restarts the fixture
daemon and regenerates relative timestamps, 30 days of activity, multiple accounts, provider health,
quota history, and pending notifications. The Swift app still reads all of this through the production
Unix-socket API; there are no mock branches in its views or view models.

For terminal-only development, point any daemon and CLI process at an isolated home:

```sh
USAGE_TRACKER_HOME="$PWD/.dev/manual" USAGE_TRACKER_FIXTURE=demo \
  cargo run -p usage-daemon

USAGE_TRACKER_HOME="$PWD/.dev/manual" cargo run -p usage-cli -- status
```

Without `USAGE_TRACKER_HOME`, `--fixture demo` and `--fixture notifications` use isolated directories
under `~/.usagetracker/fixtures/`; they do not use the production database. `USAGE_TRACKER_HOME` is a
general development hook that redirects the config, database, socket, UI config, and daemon log together.

The menu bar app's Settings page can change the polling interval, desktop usage alerts, and providers while the daemon is running; the daemon applies these immediately and persists them back to `config.json` through its `update_config` API.

Desktop alerts are enabled by default. The default thresholds are 50%, 25%, 10%, 5%, and exhausted, with durable state preventing repeats after a daemon restart. The versioned API also supports validated per-account/window threshold and reset rules, cooldowns, local quiet hours, and snooze deadlines. Synthetic or otherwise non-authoritative windows never trigger alerts. macOS asks for notification permission only when alerts are explicitly enabled. Production macOS builds must embed `usage-daemon` in the signed `UsageMenuBar.app` bundle (the supervisor looks in `Contents/MacOS`) so Notification Center can attribute and authorize it; an unbundled development daemon logs delivery failures without affecting usage collection.

On first launch, the menu app opens a setup assistant for choosing providers and connecting accounts. The same connection tools remain available under **Settings → Connections**:

- Codex opens an isolated profile login in Terminal and supports multiple named profiles.
- Claude opens `claude auth login` in Terminal.
- OpenCode Go opens its web login, then discovers available workspaces for selection.
- Grok launches `grok login` when Grok Build is installed, with grok.com sign-in as a fallback.

Provider errors include a retry or login-repair action. Account names can be changed locally in Settings. Removing an account is a reversible soft removal: collection stops and the account disappears from dashboards, while history is retained and the account can be restored.

Cost values derived from local Codex or Claude logs are estimates at API rates, not billing statements. The app labels estimated and partial totals and exposes their source in the UI.

The Unix-socket protocol is API version 2. Every request and response carries `api_version`, clients negotiate capabilities through `get_server_info`, and failures use stable machine-readable codes. Usage responses contain typed daily activity, cost, pricing coverage, provenance, and reset-credit summaries built once by the daemon. Provider-specific raw JSON is optional diagnostic data only; clients do not use it to calculate dashboard values. Refreshes are coalesced background jobs, and a repeated request joins matching in-flight work instead of queuing duplicate provider calls.

Cross-provider totals always retain their coverage context. The menu app and CLI distinguish account-wide data from this-Mac data, estimates from provider-reported values, and partial pricing coverage from complete totals. Local OpenCode history reports observed spend and activity only—it does not invent quota limits, percentages, or reset dates.

Daemon options can be passed as flags:

```sh
cargo run -p usage-daemon -- \
  --config ~/.usagetracker/config.json \
  --db-path ~/.usagetracker/usage.sqlite3 \
  --socket-path ~/.usagetracker/usage.sock \
  --log-level info
```

The same settings can be supplied with environment variables:

```sh
USAGE_TRACKER_CONFIG=~/.usagetracker/config.json \
USAGE_TRACKER_DB=~/.usagetracker/usage.sqlite3 \
USAGE_TRACKER_SOCKET=~/.usagetracker/usage.sock \
USAGE_TRACKER_LOG_LEVEL=info \
USAGE_TRACKER_POLL_INTERVAL_SECONDS=300 \
cargo run -p usage-daemon
```

The config file controls which providers are enabled:

```json
{
  "poll_interval_seconds": 300,
  "notifications": {
    "enabled": true
  },
  "providers": {
    "codex": {
      "enabled": true,
      "profiles": [
        {
          "id": "default",
          "display_name": "Personal",
          "codex_home": "~/.codex"
        }
      ]
    },
    "claude": {
      "enabled": false,
      "profiles": [
        {
          "id": "default",
          "keychain_account": "your-macos-user",
          "credentials_file": "~/.claude/.credentials.json",
          "cli_enabled": true
        }
      ]
    },
    "opencode_go": {
      "enabled": false
    },
    "grok": {
      "enabled": false,
      "profiles": [
        {
          "id": "default",
          "grok_home": "~/.grok"
        }
      ]
    }
  },
  "debug_capture_raw_payloads": false
}
```

Codex collection reads credentials from `~/.codex/auth.json`. Claude collection uses Claude Code OAuth credentials from the macOS Keychain item `Claude Code-credentials`, refreshes expired OAuth tokens, and queries Anthropic's OAuth usage API first. If that request fails for a reason other than rate limiting and `cli_enabled` is true, it falls back to the bounded local command `claude -p /usage --output-format json --no-session-persistence`.

Codex, Claude, and Grok can track multiple accounts with provider profiles. Existing configs without `profiles` keep the legacy single-account behavior. The menu bar app's Add account action creates isolated profile directories. Use the terminal button on a Claude account row to open an interactive session in that profile; its local activity stays separate and refreshes automatically. For manual configuration, Codex profiles should use separate `codex_home` or `auth_path` values; Claude profiles should use separate `claude_config_dir` values and launch sessions with the matching `CLAUDE_CONFIG_DIR`; Grok profiles should use separate `grok_home` values. In explicit Claude multi-profile configs, `cli_enabled` defaults to true only for the first profile unless it is set per profile.

Account labels are independent from provider identity. A configured or UI-edited `display_name` is preserved across refreshes and daemon restarts. Profiles without a name receive a short stable label such as `Codex 1`, `Claude 1`, or `OpenCode Go`; provider email addresses are stored separately and shown as secondary identity text.

Example multi-account config:

```json
{
  "poll_interval_seconds": 300,
  "providers": {
    "codex": {
      "enabled": true,
      "profiles": [
        {
          "id": "personal",
          "display_name": "Personal",
          "codex_home": "~/.codex"
        },
        {
          "id": "work",
          "display_name": "Work",
          "codex_home": "~/.codex-work"
        }
      ]
    },
    "claude": {
      "enabled": true,
      "profiles": [
        {
          "id": "personal",
          "keychain_account": "your-macos-user",
          "credentials_file": "~/.claude/.credentials.json",
          "cli_enabled": true
        },
        {
          "id": "work",
          "display_name": "Work",
          "keychain_account": "your-macos-user",
          "claude_config_dir": "~/.claude-work",
          "credentials_file": "~/.claude-work/.credentials.json",
          "cli_enabled": true,
          "project_roots": ["~/.claude-work/projects"]
        }
      ]
    },
    "grok": {
      "enabled": true,
      "profiles": [
        {
          "id": "default",
          "display_name": "Personal",
          "grok_home": "~/.grok"
        },
        {
          "id": "work",
          "display_name": "Work",
          "grok_home": "~/.usagetracker/profiles/grok/work"
        }
      ]
    }
  },
  "debug_capture_raw_payloads": false
}
```

Quota/rate-limit usage is collected per profile. Local cost estimates are also profile-scoped when separate Codex homes or Claude project roots are configured. During migration, a sole active managed Claude profile becomes the durable owner of existing `~/.claude` activity; additional profiles only scan their isolated roots, preventing duplication. For Claude's default `~/.claude` profile, omit `claude_config_dir` to retain the legacy unsuffixed Keychain service.

OpenCode Go collection is disabled by default. `opencode_go` tries authenticated web console usage first, then falls back to the local OpenCode SQLite database at `~/.local/share/opencode/opencode.db` when web collection is unavailable. Zen balance is fetched as optional best-effort enrichment.

Grok collection is disabled by default. `grok` first uses the official Grok Build ACP process and
its billing extension, then falls back to grok.com's account-wide billing RPC using the existing
Grok token and/or a browser session. Provider rate limits never fall back, and local Grok session
tokens are not presented as quota. See [docs/grok.md](docs/grok.md) for the transport, credential,
fallback, and security details. Additional accounts are CLI-backed and receive isolated
`GROK_HOME` directories. Browser-only login remains limited to the legacy `default` profile because
Chrome cookies cannot be reliably bound to separate Grok identities.

Grok source selection can be pinned when diagnosing either transport:

```json
"grok": {
  "enabled": true,
  "source_mode": "auto"
}
```

Supported values are `auto` (CLI then web fallback), `cli`, and `web`.

OpenCode web collection resolves cookies automatically:

1. Use a manually supplied cookie header from config, environment, or file.
2. Use the cached cookie header from the macOS Keychain.
3. Import `auth` and `__Host-auth` cookies from supported browser stores and cache the filtered header in Keychain.

The browser importer scans supported browser cookie stores for `opencode.ai` and `app.opencode.ai`. `opencode_go` checks Chrome, Dia, Firefox, Brave, Edge, Arc, Chromium, and Vivaldi.

Manual cookies are optional overrides:

```json
"opencode_go": {
  "enabled": true,
  "cookie_header": "auth=...; __Host-auth=...",
  "workspace_id": "wrk_..."
}
```

Environment variables:

```sh
USAGE_TRACKER_OPENCODE_GO_COOKIE='auth=...; __Host-auth=...'
USAGE_TRACKER_OPENCODE_GO_WORKSPACE_ID='wrk_...'
```

Cookie files:

- `~/.usagetracker/opencode_go.cookie`

With the daemon running, use the CLI from another terminal:

```sh
cargo run -p usage-cli --
```

That default command is the same as `usage` and renders the latest stored usage dashboard.

CLI commands:

```sh
cargo run -p usage-cli -- status
cargo run -p usage-cli -- usage
cargo run -p usage-cli -- usage --provider codex --account ACCOUNT_ID
cargo run -p usage-cli -- usage --details
cargo run -p usage-cli -- usage --all-providers
cargo run -p usage-cli -- --max-width 72
cargo run -p usage-cli -- --color always
cargo run -p usage-cli -- --style compact
cargo run -p usage-cli -- --style json
cargo run -p usage-cli -- refresh
cargo run -p usage-cli -- refresh --provider codex
cargo run -p usage-cli -- accounts
cargo run -p usage-cli -- accounts list --verbose
cargo run -p usage-cli -- accounts add codex --name Work
cargo run -p usage-cli -- accounts rename ACCOUNT_ID "Work account"
cargo run -p usage-cli -- accounts hide ACCOUNT_ID
cargo run -p usage-cli -- accounts disable ACCOUNT_ID
cargo run -p usage-cli -- accounts remove ACCOUNT_ID
cargo run -p usage-cli -- accounts delete ACCOUNT_ID --yes
cargo run -p usage-cli -- accounts launch ACCOUNT_ID
cargo run -p usage-cli -- providers
cargo run -p usage-cli -- providers enable claude
cargo run -p usage-cli -- providers setup opencode_go
cargo run -p usage-cli -- providers workspace opencode_go wrk_...
cargo run -p usage-cli -- providers repair codex --account ACCOUNT_ID
cargo run -p usage-cli -- config set --poll-interval 300 --notifications on
```

All commands support `--style dashboard`, `--style compact`, and `--style json`.
The dashboard fits its boxes to the terminal width, capped at 80 columns by default. Use `--max-width COLUMNS` or `USAGE_TRACKER_MAX_WIDTH` to change the cap (minimum 60 columns). `usage --details` adds per-window sub-limits, credits, forecast, and identity to each panel, and `accounts list --verbose` adds the profile and external-ID columns.
Color defaults to `--color auto`, can be forced with `--color always`, disabled with `--color never`, and respects `NO_COLOR`.
`--style json` emits the daemon's stable response shape for scripting. `usage --provider` and `--account` are repeatable. Account listings include the stable account IDs used by the management commands; `accounts remove` retains history, while `accounts delete --yes` permanently deletes it.

The CLI also defaults to `~/.usagetracker/usage.sock`. If the daemon is listening on a non-default socket, point the CLI at it:

```sh
cargo run -p usage-cli -- --socket-path ~/.usagetracker/usage.sock status
```

or:

```sh
USAGE_TRACKER_SOCKET=~/.usagetracker/usage.sock cargo run -p usage-cli -- status
```

After installing or wrapping the CLI as `usage`, the commands are the same without the `cargo run` prefix:

```sh
usage status
usage refresh --provider codex
usage accounts
```
