# usagetracker

local ai usage reports


Goal output:

```
┌─ Overview ────────────────────────────────────────────┐
│ Lifetime tokens   1.2B       Peak tokens     208.1M   │
│ Longest task      57m 39s    Current streak  19 days  │
│ Longest streak    19 days                             │
└───────────────────────────────────────────────────────┘

┌─ Activity · last 7 days ──────────────────────────────┐
│ Mon     12M  ░░░░░░░░░░░░                             │
│ Tue     44M  ██░░░░░░░░░░                             │
│ Wed     91M  █████░░░░░░░                             │
│ Thu    208M  ████████████                             │
│ Fri     88M  █████░░░░░░░                             │
│ Sat     53M  ███░░░░░░░░░                             │
│ Sun    156M  █████████░░░                             │
└───────────────────────────────────────────────────────┘

┌─ Codex · openai-web · Pro 5x ─────────────────────────┐
│ Session   67% left  ████████░░░░  resets 2:36 PM      │
│ Weekly    60% left  ███████░░░░░  resets 1:49 PM      │
│ Pace      on track   40% used vs 42% expected         │
│ Forecast  lasts      until reset                      │
│ Credits    0 left    empty                            │
│ Account   joeymalvinni@gmail.com                      │
└───────────────────────────────────────────────────────┘

┌─ Claude 2.1.173 · web ────────────────────────────────┐
│ Session  100% left  ████████████  resets in 4h        │
│ Weekly   100% left  ████████████  resets in 5d 6h     │
│ Pace     under      0% used vs 25% expected           │
│ Forecast lasts      until reset                       │
│ Account  joey_m@clovegrowth.com                       │
└───────────────────────────────────────────────────────┘
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

Codex collection reads credentials from `~/.codex/auth.json`. Claude collection reads Claude Code OAuth credentials from the macOS Keychain item `Claude Code-credentials`, refreshes expired OAuth tokens, and collects live quota usage from Anthropic's OAuth usage API.

With the daemon running, use the CLI from another terminal:

```sh
cargo run -p usage-cli --
```

That default command is the same as `usage` / `status` and returns the latest stored usage snapshot.

CLI commands:

```sh
cargo run -p usage-cli -- status
cargo run -p usage-cli -- usage
cargo run -p usage-cli -- refresh
cargo run -p usage-cli -- refresh --provider codex
cargo run -p usage-cli -- health
cargo run -p usage-cli -- accounts
cargo run -p usage-cli -- config
```

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
