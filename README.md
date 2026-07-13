<p align="center">
  <img src="apps/UsageMenuBar/AppIcon.icon/Assets/logo.svg" alt="UsageTracker logo" width="180" />
</p>

<h1 align="center">UsageTracker</h1>

<p align="center">
  Local usage, limits, resets, and cost estimates for AI coding tools.
</p>

<p align="center">
  <a href="#providers">Providers</a>
  <span>&nbsp;&nbsp;•&nbsp;&nbsp;</span>
  <a href="#build-from-source">Build</a>
  <span>&nbsp;&nbsp;•&nbsp;&nbsp;</span>
  <a href="#cli">CLI</a>
  <span>&nbsp;&nbsp;•&nbsp;&nbsp;</span>
  <a href="#configuration">Configuration</a>
  <span>&nbsp;&nbsp;•&nbsp;&nbsp;</span>
  <a href="#protocol">Protocol</a>
</p>

UsageTracker is a macOS 14+ menu bar app, background daemon, and terminal CLI. It collects usage and rate-limit data from tools you already use, stores normalized history in a local SQLite database, and serves both clients over a private Unix socket.

No hosted service is required. Provider passwords are not collected, and raw provider responses are not persisted.

## What it shows

- Session, weekly, monthly, credit, and other provider-defined usage windows.
- Reset times, remaining capacity, and usage forecasts.
- Account-wide activity when a provider exposes it.
- Local token activity and estimated cost where provider billing data is unavailable.
- Multiple Codex, Claude, and Grok accounts with isolated provider profiles.
- Provider health, authentication failures, backoff, and stale-data status.
- Desktop alerts for low or exhausted authoritative quota windows.

UsageTracker keeps provenance attached to its totals. Account-wide usage is distinguished from activity observed on this Mac, and estimated costs are not presented as provider billing statements.

## Providers

| Provider | Collection | Accounts | Default |
| --- | --- | --- | --- |
| [Codex](docs/codex.md) | Codex app-server, with ChatGPT usage fallback and local cost estimates | Multiple isolated `CODEX_HOME` profiles | Enabled |
| [Claude](docs/claude.md) | Anthropic OAuth usage API, with a bounded Claude CLI fallback and local cost estimates | Multiple isolated `CLAUDE_CONFIG_DIR` profiles | Disabled |
| [OpenCode Go](docs/opencode.md) | OpenCode web console, with local SQLite activity fallback | One web identity and workspace | Disabled |
| [Grok](docs/grok.md) | Grok Build billing RPC, with grok.com billing fallback | Multiple CLI profiles; browser login is limited to the default profile | Disabled |

Collection uses existing provider credentials from Keychain items, provider config files, or supported browser sessions. The exact source and fallback rules differ by provider; the linked provider references are authoritative.

## Build from source

Requirements:

- macOS 14 or newer.
- Rust 1.86 or newer.
- Xcode with the macOS and Icon Composer build tools.
- [`just`](https://just.systems/) for the shortest commands. Cargo and Swift commands also work directly.

Build and open the development menu bar app:

```sh
git clone https://github.com/joeymalvinni/usagetracker.git
cd usagetracker
just app
```

The app bundle is written to `apps/UsageMenuBar/.build/UsageMenuBar-dev.app`. It includes the Rust daemon and is ad-hoc signed for local development.

On first launch, choose the providers you use and follow the connection action for each account. The same controls remain available under **Settings → Connections**.

### Run the daemon and CLI directly

Run the daemon in one terminal:

```sh
cargo run -p usage-daemon
```

Then make the first request from another terminal:

```sh
cargo run -p usage-cli -- status
```

The daemon creates its config, database, and socket automatically. Codex is enabled by default; the other providers remain disabled until enabled in the app, CLI, or config file.

## CLI

The default command renders the current usage dashboard:

```sh
cargo run -p usage-cli --
```

Common commands:

```sh
cargo run -p usage-cli -- status
cargo run -p usage-cli -- usage --details
cargo run -p usage-cli -- usage --provider codex
cargo run -p usage-cli -- refresh
cargo run -p usage-cli -- refresh --provider claude
cargo run -p usage-cli -- accounts
cargo run -p usage-cli -- providers
cargo run -p usage-cli -- config show
```

Account management:

```sh
cargo run -p usage-cli -- accounts add codex --name Work
cargo run -p usage-cli -- accounts rename ACCOUNT_ID "Work account"
cargo run -p usage-cli -- accounts hide ACCOUNT_ID
cargo run -p usage-cli -- accounts disable ACCOUNT_ID
cargo run -p usage-cli -- accounts remove ACCOUNT_ID
cargo run -p usage-cli -- accounts delete ACCOUNT_ID --yes
```

Provider and live configuration changes:

```sh
cargo run -p usage-cli -- providers enable claude
cargo run -p usage-cli -- providers setup opencode_go
cargo run -p usage-cli -- providers repair codex --account ACCOUNT_ID
cargo run -p usage-cli -- config set --poll-interval 300 --notifications on
```

Pass `--style json` for machine-readable output:

```sh
cargo run -p usage-cli -- --style json usage --provider codex
```

CLI JSON is a command-level interface, not a raw socket response. It omits the socket envelope, and commands such as `status` use their own output shape. Do not assume that `--style json` includes `api_version` or exactly mirrors the daemon API.

Use `--help` on any command for its complete flags and subcommands:

```sh
cargo run -p usage-cli -- accounts --help
```

## Configuration

The default configuration lives at `~/.usagetracker/config.json`:

```json
{
  "poll_interval_seconds": 300,
  "notifications": {
    "enabled": true
  },
  "providers": {
    "codex": { "enabled": true },
    "claude": { "enabled": false },
    "opencode_go": { "enabled": false },
    "grok": { "enabled": false }
  }
}
```

The polling interval must be at least 60 seconds. Changes made through the menu app or `config set` are validated, applied immediately, and persisted atomically. Direct edits to `config.json` require a daemon restart.

Runtime paths can be overridden without changing the file:

```sh
USAGE_TRACKER_CONFIG=/path/to/config.json \
USAGE_TRACKER_DB=/path/to/usage.sqlite3 \
USAGE_TRACKER_SOCKET=/path/to/usage.sock \
cargo run -p usage-daemon
```

`USAGE_TRACKER_HOME` redirects the config, database, socket, UI preferences, and daemon log together. It is useful for isolated development environments:

```sh
USAGE_TRACKER_HOME="$PWD/.dev/manual" cargo run -p usage-daemon
```

Provider profiles and credential overrides are provider-specific. See [Codex](docs/codex.md), [Claude](docs/claude.md), [OpenCode Go](docs/opencode.md), and [Grok](docs/grok.md) before editing those fields manually.

## Local files and privacy

UsageTracker keeps its state under `~/.usagetracker/` by default:

| Path | Contents |
| --- | --- |
| `config.json` | Provider settings, profile paths, notifications, and optional manual cookie overrides |
| `usage.sqlite3` | Accounts, normalized usage snapshots, daily history, health, backoff, and notification state |
| `usage.sock` | Local daemon socket |
| `usage-daemon.log` | Rotated daemon output when launched by the menu app |
| `ui/config.json` | Menu bar presentation preferences |

The app directory is restricted to the current user, and the config and socket use owner-only permissions. This protects data from other local users, but not from another process already running as the same macOS user.

Credential handling depends on the provider:

- Keychain credentials remain in Keychain and are read when needed.
- Provider credential files are read from their known or configured locations.
- Browser-cookie collection reads only the supported provider domains and stores filtered cached headers in Keychain where implemented.
- A manually configured `cookie_header` is stored in `config.json`; protect backups and diagnostic bundles that include it.

Raw provider payloads are normalized in memory and are not stored. Sanitized diagnostics may be stored with normalized snapshots and returned to local clients.

High-resolution snapshots retain up to 90 days and 10,000 rows per account. Daily usage history is retained until the account is permanently deleted. Hiding, disabling, or removing an account preserves its history; `accounts delete --yes` permanently deletes the account and associated usage data.

## Protocol

The daemon listens on `~/.usagetracker/usage.sock` and exchanges newline-delimited JSON over a persistent Unix socket connection.

- Current protocol version: `3`.
- Every socket request and response includes `api_version`.
- Client and daemon versions must match exactly.
- Requests are limited to 64 KiB.
- Responses are limited to 8 MiB.
- Multiple requests may be pipelined; responses preserve request order.
- Refreshes run as background jobs and matching in-flight work is coalesced.
- Refresh jobs are in memory and do not survive a daemon restart. The most recent 64 completed jobs remain queryable.

Minimal exchange:

```sh
printf '%s\n' '{"api_version":3,"method":"get_server_info"}' \
  | nc -U ~/.usagetracker/usage.sock
```

The Rust wire types in [`usage-core`](crates/usage-core/src/api.rs) define the protocol. Checked-in examples live under [`crates/usage-core/wire-fixtures`](crates/usage-core/wire-fixtures/).

## Development

The workspace contains three Rust crates and one Swift package:

```text
crates/usage-core/      shared protocol and models
crates/usage-daemon/    collection, storage, polling, and socket server
crates/usage-cli/       terminal client
apps/UsageMenuBar/      macOS SwiftUI menu bar app
```

Useful commands:

```sh
just build               # Rust workspace and development app bundle
just app                 # build and open the development app
just daemon              # run the daemon in the foreground
just cli status          # run a CLI command
just test                # run Rust tests
just check               # Rust, Swift, formatting, Clippy, and audit checks
```

Launch the app with a reset synthetic database:

```sh
just fixture
just fixture notifications
```

Fixture state is stored under `.dev/fixture/` and never reads or replaces the normal `~/.usagetracker` database.

Direct verification commands:

```sh
cargo test --workspace --all-features
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
swift test --package-path apps/UsageMenuBar -Xswiftc -strict-concurrency=complete
```

## License

[MIT](LICENSE) © 2026 Joey Malvinni and UsageTracker contributors.
