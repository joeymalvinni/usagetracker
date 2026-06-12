# Rust Daemon Implementation Guide

This document turns the daemon plan into an implementation path. The daemon should be a small Rust service that owns provider collection, credential access, storage, polling, and the local API used by every frontend.

The first version should optimize for boring correctness:

* One daemon process.
* One SQLite database.
* One local JSON-over-Unix-socket API.
* One provider implemented end to end.
* Clear health and error reporting.
* Enough structure to add more providers without rewriting the runtime.

## Goals

The daemon is responsible for:

* Loading config.
* Discovering enabled provider accounts.
* Reading local credentials.
* Polling provider usage endpoints.
* Normalizing provider responses.
* Persisting snapshots and provider health.
* Serving current state to local clients.
* Handling manual refresh requests.

The daemon should not render terminal output, menu bar UI, or widget UI. Those are client concerns.

## Crate Layout

Start with a Rust workspace so the daemon, CLI, and shared types can evolve together:

```text
usagetracker/
  Cargo.toml
  crates/
    usage-core/
      src/
        lib.rs
        ids.rs
        model.rs
        api.rs
        error.rs
    usage-daemon/
      src/
        main.rs
        config.rs
        daemon.rs
        polling.rs
        server.rs
        storage.rs
        health.rs
        providers/
          mod.rs
          codex.rs
          claude.rs
    usage-cli/
      src/
        main.rs
```

Suggested dependency split:

* `usage-core`: shared API request/response types, normalized usage models, IDs, and stable errors.
* `usage-daemon`: provider collectors, SQLite, credentials, polling, and Unix socket server.
* `usage-cli`: terminal commands that talk to the daemon.

Keep provider-specific response structs inside `usage-daemon`. Only normalized models should cross into `usage-core`.

## Dependencies

Use a conservative async stack:

```toml
[dependencies]
tokio = { version = "1", features = ["macros", "rt-multi-thread", "net", "signal", "time", "fs"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
thiserror = "2"
anyhow = "1"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
reqwest = { version = "0.12", features = ["json", "rustls-tls"], default-features = false }
sqlx = { version = "0.8", features = ["runtime-tokio", "sqlite", "migrate", "chrono", "json"] }
chrono = { version = "0.4", features = ["serde"] }
uuid = { version = "1", features = ["serde", "v4"] }
clap = { version = "4", features = ["derive", "env"] }
dirs = "6"
```

Add `keyring` later when the first Keychain-backed provider needs it. File-backed credential loading is enough for the first Codex collector.

## Runtime Shape

`usage-daemon` should start in this order:

1. Initialize tracing.
2. Load config from default paths and environment overrides.
3. Open or create SQLite.
4. Run migrations.
5. Build provider collectors.
6. Start the Unix socket server.
7. Run an initial refresh.
8. Start the polling loop.
9. Wait for shutdown signals.

The daemon should support foreground development first:

```text
usage-daemon --foreground --log-level debug
```

LaunchAgent installation can come later once the foreground process is stable.

## Config

Use a small config file, but allow useful environment overrides for development.

Temporary default paths for testing:

```text
config: ./config.json (for now)
db:     ./usage.sqlite3
socket: ./usage.sock
logs:   ./daemon.log
```

Initial config shape:

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
  }
}
```

Keep config parsing strict enough to catch mistakes, but do not make missing config fatal. The daemon can create defaults on first run.

## Normalized Model

The model should represent provider data without pretending every provider exposes the same fields.

Core types in `usage-core`:

```rust
pub struct UsageSnapshot {
    pub provider_id: ProviderId,
    pub account_id: AccountId,
    pub collected_at: DateTime<Utc>,
    pub windows: Vec<UsageWindow>,
    pub metadata: serde_json::Value,
}

pub struct UsageWindow {
    pub window_id: String,
    pub label: String,
    pub kind: UsageWindowKind,
    pub used: Option<UsageAmount>,
    pub limit: Option<UsageAmount>,
    pub remaining: Option<UsageAmount>,
    pub percent_used: Option<f64>,
    pub percent_remaining: Option<f64>,
    pub reset_at: Option<DateTime<Utc>>,
}

pub enum UsageWindowKind {
    Session,
    Daily,
    Weekly,
    Monthly,
    Credits,
    Tokens,
    Other(String),
}

pub struct UsageAmount {
    pub value: f64,
    pub unit: UsageUnit,
}

pub enum UsageUnit {
    Tokens,
    Requests,
    Credits,
    Percent,
    Unknown,
}
```

Rules:

* Optional fields stay optional.
* Do not estimate reset times unless the provider gives enough data.
* Keep raw provider payloads out of normal client responses.
* Preserve provider-specific details in `metadata` only when they are useful for diagnostics.

## Storage

Use migrations checked into the daemon crate:

```text
crates/usage-daemon/migrations/
  0001_initial.sql
```

Initial schema:

```sql
CREATE TABLE accounts (
  id TEXT PRIMARY KEY,
  provider_id TEXT NOT NULL,
  external_account_id TEXT NOT NULL,
  display_name TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  UNIQUE(provider_id, external_account_id)
);

CREATE TABLE usage_snapshots (
  id TEXT PRIMARY KEY,
  provider_id TEXT NOT NULL,
  account_id TEXT NOT NULL,
  collected_at TEXT NOT NULL,
  normalized_json TEXT NOT NULL,
  metadata_json TEXT,
  FOREIGN KEY(account_id) REFERENCES accounts(id)
);

CREATE INDEX usage_snapshots_provider_account_time
ON usage_snapshots(provider_id, account_id, collected_at DESC);

CREATE TABLE raw_payloads (
  id TEXT PRIMARY KEY,
  snapshot_id TEXT NOT NULL,
  provider_id TEXT NOT NULL,
  collected_at TEXT NOT NULL,
  payload_json TEXT NOT NULL,
  FOREIGN KEY(snapshot_id) REFERENCES usage_snapshots(id)
);

CREATE TABLE provider_health (
  provider_id TEXT NOT NULL,
  account_id TEXT,
  status TEXT NOT NULL,
  collection_mode TEXT,
  last_success_at TEXT,
  last_failure_at TEXT,
  last_error_code TEXT,
  last_error_message TEXT,
  updated_at TEXT NOT NULL,
  PRIMARY KEY(provider_id, account_id)
);
```

Store normalized snapshots as JSON at first. That keeps the first implementation small and lets the model settle. Add query-optimized columns later only when the CLI or UI needs them.

## Provider Collector Trait

Provider collectors should have one job: produce normalized snapshots from provider-specific credentials and responses.

```rust
#[async_trait::async_trait]
pub trait ProviderCollector: Send + Sync {
    fn provider_id(&self) -> ProviderId;

    async fn discover_accounts(&self) -> Result<Vec<DiscoveredAccount>, ProviderError>;

    async fn collect_usage(
        &self,
        account: &DiscoveredAccount,
    ) -> Result<ProviderCollectionResult, ProviderError>;
}
```

`ProviderCollectionResult` should contain:

* Normalized snapshot data.
* Collection mode.
* Optional raw payload for debug storage.
* Warnings that should update provider health but not fail the entire refresh.

Provider errors should be typed:

```rust
pub enum ProviderErrorKind {
    CredentialsMissing,
    CredentialsInvalid,
    Unauthorized,
    RateLimited,
    Network,
    Parse,
    ProviderUnavailable,
    Unsupported,
}
```

Do not let one provider failure fail the whole daemon refresh. Record health, continue with other providers, and return partial results to clients.

## Codex First

Implement Codex first because it is simpler.

Credential path:

```text
~/.codex/auth.json
```

Read:

```text
.tokens.access_token
.tokens.account_id
```

Request:

```text
GET https://chatgpt.com/backend-api/wham/usage
Authorization: Bearer <token>
ChatGPT-Account-Id: <account_id>
Accept: application/json
User-Agent: codex-cli
```

Implementation notes:

* Treat missing `auth.json` as `CredentialsMissing`.
* Treat missing token fields as `CredentialsInvalid`.
* Store the account ID as the external account ID.
* Store the raw response only when debug raw payload capture is enabled.
* Normalize only fields that are directly present in the response.
* If the response shape changes, return `Parse` and preserve enough context in logs.

## Polling

Use one polling coordinator instead of letting providers schedule themselves.

Responsibilities:

* Run refresh on startup.
* Run refresh every `poll_interval_seconds`.
* Accept manual refresh requests from the local API.
* Coalesce concurrent refresh requests.
* Apply per-provider backoff after failures.
* Update provider health after each account refresh.

Use a `tokio::sync::watch` channel for latest state and a `tokio::sync::mpsc` channel for refresh requests.

Manual refresh should return after the refresh completes:

```text
client -> refresh
daemon -> RefreshResponse { started_at, finished_at, provider_results }
```

If a refresh is already running, either await the active refresh or mark the request as coalesced. Do not start duplicate HTTP calls for the same provider/account pair.

## Local API

Use a Unix domain socket with newline-delimited JSON. This is easy to debug with simple tools and keeps framing simple.

Initial methods:

```text
get_usage
refresh -- pass in providers to refresh, or default to all
get_provider_health
get_accounts
get_config
```

Use request and response enums in `usage-core` so the CLI and daemon compile against the same contract.

Server behavior:

* One JSON request per line.
* One JSON response per line.
* Invalid JSON returns a structured error.
* Unknown method returns a structured error.
* Client disconnects should not affect the daemon.
* Socket file should be removed and recreated on daemon start if stale.

## Health

Health is part of the product. A user should be able to tell whether usage is missing because they are under quota, credentials failed, or the provider endpoint changed.

Track per provider and account:

* `ok`
* `credentials_missing`
* `auth_failed`
* `rate_limited`
* `provider_error`
* `parse_error`
* `backing_off`
* `disabled`

Include:

* Last successful refresh time.
* Last failed refresh time.
* Last error code.
* Last short error message.
* Collection mode.

Keep detailed error chains in logs. Keep client-facing errors short and actionable.

## Logging

Use `tracing`.

Required fields on collection spans:

* provider ID
* account ID
* collection mode
* refresh request ID

Never log access tokens, refresh tokens, cookies, or full auth files. Add a helper for redacting known sensitive strings before logging provider errors.

## Testing

Start with tests around the boundaries most likely to break:

* Config defaulting.
* SQLite migrations.
* Provider credential parsing.
* Provider response normalization using fixtures.
* API request/response serialization.
* Polling coalescing behavior.

Use fixture files for provider payloads:

```text
crates/usage-daemon/fixtures/codex/usage-success.json
crates/usage-daemon/fixtures/codex/usage-unexpected-shape.json
```

Provider tests should not call live provider endpoints. Keep live calls behind an ignored integration test or a manual debug command.

## Build Order

1. Create the Cargo workspace and empty crates.
2. Add `usage-core` models and API request/response enums.
3. Add daemon config loading and path resolution.
4. Add SQLite open, migrations, and storage methods.
5. Add a Unix socket server with `get_config` and `get_provider_health`.
6. Add the Codex credential loader.
7. Add the Codex HTTP collector.
8. Normalize Codex usage into `UsageSnapshot`.
9. Persist snapshots and health in sqlite.
10. Add `get_usage`.
11. Add `refresh`.
12. Add a minimal CLI client (logging json responses from daemon).
14. Package foreground daemon usage in CLI help.

This order keeps every step runnable and avoids building a generic framework before one provider works.

## Development Commands

Useful commands once the workspace exists:

```text
cargo fmt
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p usage-daemon -- --foreground --log-level debug
cargo run -p usage-cli -- status
cargo run -p usage-cli -- refresh
```

Add these commands to CI after the workspace lands.

## LaunchAgent
To be implemented much later: After foreground mode works, add a LaunchAgent installer command:

```text
usage-daemon install-launch-agent
usage-daemon uninstall-launch-agent
```

The generated plist should point at the built daemon binary and use the default config, socket, database, and log paths. Keep this as a later milestone because debugging provider and socket behavior is easier in foreground mode.