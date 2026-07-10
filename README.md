# usagetracker

local ai usage reports


```
┌─ Overview ─────────────────────────────────────────────────┐
│ Lifetime tokens  3.8B       Peak tokens    167.9M          │
│ Longest task     n/a        Current streak 2 days          │
│ Longest streak   8 days                                    │
└────────────────────────────────────────────────────────────┘

┌─ Activity · last 7 days ───────────────────────────────────┐
│ Wed    1.6M  █░░░░░░░░░░░                                  │
│ Thu   81.5M  ████████████                                  │
│ Fri   30.9M  ████░░░░░░░░                                  │
│ Sat    1.6M  █░░░░░░░░░░░                                  │
│ Sun       0  ░░░░░░░░░░░░                                  │
│ Mon  106.3K  █░░░░░░░░░░░                                  │
│ Tue   84.1M  ████████████                                  │
└────────────────────────────────────────────────────────────┘

┌─ Claude · web · Team ──────────────────────────────────────┐
│ Session   80% left  ██████████░░  resets in 2h 1m          │
│ Weekly   100% left  ████████████  resets in 6d 22h         │
│ Pace     on track     0% used vs   1% expected             │
│ Forecast lasts      until reset                            │
│ Account  joey                                              │
└────────────────────────────────────────────────────────────┘

┌─ Codex · openai-web · Pro Lite ────────────────────────────┐
│ Session   82% left  ██████████░░  resets in 3h 20m         │
│ Weekly    87% left  ██████████░░  resets in 6d 12h         │
│ Pace     over        13% used vs   7% expected             │
│ Forecast tight       before reset                          │
│ Credits  0 left    empty                                   │
│ Account  joeymalvinni@gmail.com                            │
└────────────────────────────────────────────────────────────┘
```

## Usage

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

The menu bar app's Settings page can change the polling interval and enable/disable providers while the daemon is running; the daemon applies these immediately and persists them back to `config.json` through its `update_config` API.

On first launch, the menu app opens a setup assistant for choosing providers and connecting accounts. The same connection tools remain available under **Settings → Connections**:

- Codex opens an isolated profile login in Terminal and supports multiple named profiles.
- Claude opens `claude auth login` in Terminal.
- OpenCode Go opens its web login, then discovers available workspaces for selection.

Provider errors include a retry or login-repair action. Account names can be changed locally in Settings. Removing an account is a reversible soft removal: collection stops and the account disappears from dashboards, while history is retained and the account can be restored.

Cost values derived from local Codex or Claude logs are estimates at API rates, not billing statements. The app labels estimated and partial totals and exposes their source in the UI.

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
    }
  },
  "debug_capture_raw_payloads": false
}
```

Codex collection reads credentials from `~/.codex/auth.json`. Claude collection defaults to the local Claude Code terminal usage command, `claude -p /usage --output-format json --no-session-persistence`. If that command fails, Claude collection falls back to Claude Code OAuth credentials from the macOS Keychain item `Claude Code-credentials`, refreshes expired OAuth tokens, and collects quota usage from Anthropic's OAuth usage API.

Codex and Claude can track multiple accounts with provider profiles. Existing configs without `profiles` keep the legacy single-account behavior. For Codex, each profile should point at a separate `codex_home` or `auth_path`. For Claude, each profile can point at a separate Keychain account or credentials file. In explicit Claude multi-profile configs, `cli_enabled` defaults to true only for the first profile so local Claude CLI usage and local project log costs are not duplicated across accounts.

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
          "cli_enabled": true
        },
        {
          "id": "work",
          "keychain_account": "joey-work",
          "cli_enabled": false
        }
      ]
    }
  },
  "debug_capture_raw_payloads": false
}
```

Quota/rate-limit usage is collected per profile. Local cost estimates are also profile-scoped when separate Codex homes or Claude project roots are configured; otherwise only the CLI-enabled Claude profile receives local log cost enrichment to avoid double-counting.

OpenCode Go collection is disabled by default. `opencode_go` tries authenticated web console usage first, then falls back to the local OpenCode SQLite database at `~/.local/share/opencode/opencode.db` when web collection is unavailable. Zen balance is fetched as optional best-effort enrichment.

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

That default command is the same as `usage` / `status` and renders the latest stored usage dashboard.

CLI commands:

```sh
cargo run -p usage-cli -- status
cargo run -p usage-cli -- usage
cargo run -p usage-cli -- --color always
cargo run -p usage-cli -- --style compact
cargo run -p usage-cli -- --style json
cargo run -p usage-cli -- refresh
cargo run -p usage-cli -- refresh --provider codex
cargo run -p usage-cli -- health
cargo run -p usage-cli -- accounts
cargo run -p usage-cli -- config
```

Usage/status output supports `--style dashboard`, `--style compact`, and `--style json`.
Color defaults to `--color auto`, can be forced with `--color always`, disabled with `--color never`, and respects `NO_COLOR`.
Other commands continue to return daemon API JSON.

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
usage health
```
