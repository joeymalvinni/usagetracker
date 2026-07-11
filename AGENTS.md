# AGENTS.md

## Project

Local AI usage tracker: background daemon polls providers (Codex/OpenAI, Claude/Anthropic, OpenCode Go) for usage/rate-limit data, stores in SQLite, exposes via Unix socket. CLI and macOS menu bar app consume the socket.

## Workspace layout

```
Cargo.toml          # Rust workspace root
crates/usage-core/  # Shared models, API types, paths (no I/O deps)
crates/usage-daemon/# Background daemon binary
crates/usage-cli/   # Terminal CLI binary
apps/UsageMenuBar/  # macOS SwiftUI menu bar app (SPM, NOT Cargo)
docs/               # Per-provider architecture docs (read these for collection logic)
```

The Swift app lives outside the Cargo workspace. `cargo build` only builds the 3 Rust crates.

## Commands

```sh
cargo build                        # Build all Rust crates
cargo run -p usage-daemon          # Run daemon (foreground, binds Unix socket)
cargo run -p usage-cli -- status   # Run CLI (connects to running daemon)
cargo test                         # Run all Rust tests
cargo test -p usage-daemon         # Run daemon tests only
cargo test -p usage-core           # Run core tests only
cargo test -p usage-cli            # Run CLI tests only
cargo test -- test_name_substring  # Run a single test by name
```

There is no CI, lint enforcement, or pre-commit hook. Tests, clippy, and fmt are run locally:

```sh
cargo clippy --all-targets
cargo fmt --all -- --check
```

## Tests

All tests are **inline** (`#[test]` functions inside `src/` files). There is no `tests/` directory. Tests use `#[cfg(test)] mod tests { ... }` style with unit-test-only imports.

When running a single test, pass a substring of the test function name to `cargo test`.

## Architecture notes

**Daemon start**: Creates `~/.usagetracker/` (config, SQLite DB, Unix socket) automatically on first run. Config defaults: codex enabled, claude/opencode_go disabled.

**SQLite schema**: `crates/usage-daemon/migrations/0001_initial.sql` is the authoritative disposable local schema and is applied transactionally. Provider data is reproducible; incompatible legacy schemas are reset instead of repaired in production code.

**Claude dual-path collection**: Attempts the OAuth usage API first using Keychain-stored credentials. On eligible failures, optionally falls back to the bounded CLI command (`claude -p /usage --output-format json --no-session-persistence`). Rate limits never fall back. Detailed logic in `docs/claude.md`.

**Provider naming quirk**: Config and CLI use `opencode_go`, but internal Rust code (server.rs, config.rs) explicitly filters out the bare `opencode` provider name in API responses. The web-enabled provider is `opencode_go` throughout user-facing surface.

**macOS only**: Daemon uses macOS Keychain for credentials, FSEvent for file watching (`notify`), and Apple-native keyring. The Swift menu bar app is macOS 14+.

**Credential sourcing** (priority order): Keychain → config file → browser cookie stores (Chrome, Dia, Firefox, Brave, Edge, Arc, Chromium, Vivaldi).

**Configuration**: Live config updates from the Swift app settings page are sent to the daemon over the socket and persisted to `config.json` immediately, including poll interval changes that take effect at runtime via a tokio watch channel.
