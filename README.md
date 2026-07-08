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
      "enabled": true
    },
    "claude": {
      "enabled": false
    }
  },
  "debug_capture_raw_payloads": false
}
```

Codex collection reads credentials from `~/.codex/auth.json`. Claude collection defaults to the local Claude Code terminal usage command, `claude -p /usage --output-format json --no-session-persistence`. If that command fails, Claude collection falls back to Claude Code OAuth credentials from the macOS Keychain item `Claude Code-credentials`, refreshes expired OAuth tokens, and collects quota usage from Anthropic's OAuth usage API.

With the daemon running, use the CLI from another terminal:

```sh
cargo run -p usage-cli --
```

That default command is the same as `usage` / `status` and renders the latest stored usage dashboard.

CLI commands:

```sh
cargo run -p usage-cli -- status
cargo run -p usage-cli -- usage
cargo run -p usage-cli -- --style compact
cargo run -p usage-cli -- --style json
cargo run -p usage-cli -- refresh
cargo run -p usage-cli -- refresh --provider codex
cargo run -p usage-cli -- health
cargo run -p usage-cli -- accounts
cargo run -p usage-cli -- config
```

Usage/status output supports `--style dashboard`, `--style compact`, and `--style json`.
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
