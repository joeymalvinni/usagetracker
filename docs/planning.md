# AI Usage Tracker Plan

The app should be built around one durable principle: **provider usage collection is the core product**, and every interface should be a view over the same local state.

The daemon owns provider integration, credential access, polling, normalization, storage, and health tracking. The CLI, macOS menu bar app, and any future widgets should not each implement their own Codex, Claude, or provider-specific logic. They should query the daemon or, in limited debug cases, read the daemon-owned SQLite store.

## Architecture

The project has two layers:

1. **Core layer**

   * Rust daemon
   * Rust provider collectors
   * SQLite storage
   * Local daemon API

2. **Interface layer**

   * Rust CLI
   * macOS Swift menu bar app
   * Optional Linux widgets such as `eww`

The daemon is the source of truth. Interfaces render data, trigger refreshes, expose settings, and surface errors.

## Implementation Language

Use **Rust** for the daemon and CLI.

Rust is a strong fit because the project needs:

* A reliable long-running background process.
* Good async HTTP support.
* Strong JSON and typed data modeling.
* SQLite integration.
* Unix domain socket support.
* Cross-platform potential.
* Safe handling of local credentials and provider state.
* A fast, native CLI binary.

Recommended Rust stack:

* `tokio` for async runtime.
* `reqwest` or `hyper` for HTTP.
* `serde` / `serde_json` for typed provider payloads.
* `sqlx` or `rusqlite` for SQLite.
* `clap` for CLI commands.
* `tracing` for logs.
* `thiserror` / `anyhow` for error handling.
* Unix domain sockets for daemon communication.
* `keyring` or macOS Security.framework bindings for credential access where appropriate.

The macOS menu bar app can remain Swift-native and communicate with the Rust daemon over the local API.

## Daemon

The daemon is the most important component. It runs in the background, refreshes provider usage, normalizes responses, stores snapshots, and exposes the latest state to clients.

Core responsibilities:

* Load configured providers and accounts.
* Read provider-specific credentials.
* Poll usage endpoints or fallback probes.
* Normalize usage into a common local model.
* Store timestamped snapshots in SQLite.
* Store optional raw provider payloads for debugging.
* Track provider health, auth failures, retry state, and last successful refresh.
* Expose current state through a local API.

During development, the daemon can run as a foreground process. On macOS, it should eventually be installed as a LaunchAgent.

Polling should start simple:

* Configurable fixed interval.
* Manual refresh command.
* Refresh on daemon startup.
* Refresh after wake from sleep, if supported.
* Backoff after provider failures.

Later, polling can become adaptive:

* Poll more frequently when the user is actively using AI tools.
* Poll less frequently when reset times are far away.
* Poll immediately after detected CLI or app activity.
* Avoid hammering unstable or rate-limited endpoints.

## CLI

The CLI should be a terminal frontend over daemon state, not a separate usage collector.

Responsibilities:

* Render overview cards, tables, and activity summaries.
* Show provider and account status.
* Query the daemon for latest normalized usage.
* Trigger manual refreshes.
* Export debug snapshots.
* Inspect provider health and errors.
* Optionally read SQLite directly for development/debugging.

Long term, normal CLI commands should talk to the daemon through the local socket. Direct SQLite reads should be reserved for diagnostic commands.

Useful CLI commands:

```text
usage
usage status
usage refresh
usage providers
usage accounts
usage activity
usage doctor
usage debug raw <provider>
```

The default output should show:

1. Overall status.
2. Recent activity.
3. One compact card per provider/account.
4. Credential or polling errors only when relevant.

## macOS Menu Bar App

The menu bar app should be a native Swift frontend over the same daemon data.

Responsibilities:

* Show compact current usage status in the menu bar.
* Provide an overview section.
* Provide provider-specific sections.
* Trigger refreshes.
* Surface credential and provider errors clearly.
* Configure enabled providers, polling interval, and credential access.
* Open debug/help views when something breaks.

The visual polish belongs in the Swift frontend. The daemon should remain boring, stable, and easy to test.

Provider tabs make sense, but the UI should start with an overview because the user usually wants to know whether they are close to a limit before they care which provider caused it.

## Linux Widgets

Linux widgets such as `eww` should be examples of the daemon contract.

They should not contain provider logic. They should call the CLI or daemon API and render the returned normalized data.

Example flow:

```text
eww widget -> usage status --json -> render compact usage module
```

## Local API

Use a Unix domain socket with JSON messages for the initial daemon API.

The first API can stay small:

```text
get_latest_usage
refresh_now
get_provider_health
get_accounts
get_config
```

The daemon should return normalized data by default. Raw provider payloads should be available only through explicit debug commands.

The daemon should be the only component that touches:

* Provider auth files.
* macOS Keychain.
* Claude credential files.
* Codex auth files.
* Provider HTTP endpoints.
* Raw provider responses.

## Storage

Use SQLite as the primary local store.

SQLite gives the app:

* Fast local reads.
* Historical snapshots.
* Simple queries.
* Durable state.
* Easy debugging.
* No external service dependency.

JSON fixtures are still useful, but only for tests, debug exports, and golden files.

Useful tables:

```text
enabled_providers
accounts
usage_snapshots
usage_limits
raw_payloads
provider_health
```

The normalized model should support common usage concepts without pretending every provider exposes the same fields.

Core concepts:

* Provider.
* Account.
* Plan or tier, if known.
* Usage window.
* Limit type.
* Used amount.
* Remaining amount.
* Percent used or left.
* Reset time.
* Collection mode.
* Last refresh time.
* Provider-specific metadata.

The model should support windows like:

```text
session
daily
weekly
monthly
credits
tokens
```

But every field should be optional where providers do not expose it.

## Provider Integration Model

Each provider should implement a common Rust trait.

Conceptually:

```rust
trait ProviderCollector {
    fn provider_id(&self) -> ProviderId;
    async fn discover_accounts(&self) -> Result<Vec<Account>>;
    async fn collect_usage(&self, account: &Account) -> Result<ProviderCollectionResult>;
}
```

Provider collectors should return both:

1. Normalized usage data.
2. Provider-specific metadata or raw payload references for debugging.

Provider-specific parsing should stay inside the provider module. The rest of the app should only depend on normalized usage structures.

## Codex / ChatGPT Provider

Codex should be the first integration because the credential source and usage endpoint are relatively straightforward.

Credential source:

```text
~/.codex/auth.json
```

Required fields:

```text
.tokens.access_token
.tokens.account_id
```

Usage endpoint:

```text
https://chatgpt.com/backend-api/wham/usage
```

Request headers:

```text
Authorization: Bearer <token>
ChatGPT-Account-Id: <account_id>
Accept: application/json
User-Agent: codex-cli
```

Related endpoints worth investigating:

```text
/backend-api/wham/usage
/backend-api/wham/accounts/check
/backend-api/wham/profiles/me
```

Important limitation: some endpoints expose aggregate profile stats but not quota cards, percent remaining, reset times, plan name, or credits. The UI should only show quota fields when the daemon has real data for them.

## Claude Provider

Claude is more complex because credential and usage access vary by platform and installation method.

Ignore Anthropic Admin API keys for the first version. They require manual setup and are not the same as reading the local Claude Code user session.

Preferred credential sources:

1. macOS Keychain entry for Claude Code credentials.
2. Disk fallback at:

```text
~/.claude/.credentials.json
```

Preferred API path:

```text
https://api.anthropic.com/api/oauth/usage
```

Headers:

```text
Authorization: Bearer <token>
anthropic-beta: oauth-2025-04-20
```

Fallback path:

```text
Run claude in a PTY, execute /usage, parse the output.
```

The PTY probe is useful, but it should be treated as unstable because terminal output can change. It is a fallback and validation path, not the preferred collector.

Claude collection should report its mode:

```text
oauth_api_keychain
oauth_api_file
cli_pty_probe
unavailable
```


The CLI, menu bar app, and widgets are views over daemon-owned state. That keeps the project easier to test, easier to extend, and safer when provider APIs or credential formats change.
