set shell := ["zsh", "-cu"]

app_dir := "apps/UsageMenuBar"
dev_app_bundle := app_dir + "/.build/UsageMenuBar-dev.app"
release_app_bundle := app_dir + "/.build/UsageMenuBar.app"
fixture_home := justfile_directory() + "/.dev/fixture"

# Show the available commands.
default:
    @just --list

# Build the Rust workspace and the development macOS app bundle.
build: build-rust build-app-dev

# Build all Rust crates.
build-rust:
    cargo build

# Build and ad-hoc sign a development macOS app bundle, including the daemon.
build-app-dev:
    ./{{app_dir}}/package-dev-app.sh debug

# Build and ad-hoc sign an optimized macOS app bundle, including the daemon.
build-app-release:
    ./{{app_dir}}/package-dev-app.sh release

# Build and launch the development macOS app bundle.
app: app-dev

# Build and launch the development macOS app bundle.
app-dev: build-app-dev
    open -n {{dev_app_bundle}}

# Build and launch the optimized macOS app bundle.
app-release: build-app-release
    open -n {{release_app_bundle}}

# Launch the app against a reset synthetic database (demo or notifications).
fixture scenario="demo": build-app-dev
    open -n --env USAGE_TRACKER_HOME="{{fixture_home}}" --env USAGE_TRACKER_FIXTURE="{{scenario}}" {{dev_app_bundle}}

# Run the daemon in the foreground; pass daemon flags after the recipe name.
daemon *args:
    cargo run -p usage-daemon -- {{args}}

# Run the CLI; for example, `just cli status`.
cli *args:
    cargo run -p usage-cli -- {{args}}

# Run all Rust tests.
test *args:
    cargo test {{args}}

# Run Clippy for every Rust target.
clippy:
    cargo clippy --workspace --all-targets --all-features -- -D warnings

# Format Rust sources.
fmt:
    cargo fmt --all

# Verify Rust formatting without changing files.
fmt-check:
    cargo fmt --all -- --check

# Run Rust checks with the same flags as CI.
check-rust: fmt-check clippy
    cargo test --workspace --all-features

# Build and test Swift with CI's strict-concurrency setting.
check-swift:
    swift build --package-path {{app_dir}} -Xswiftc -strict-concurrency=complete
    swift test --package-path {{app_dir}} -Xswiftc -strict-concurrency=complete

# Check dependencies against the RustSec advisory database.
audit:
    cargo audit

# Run the full local verification suite (both CI jobs).
check: check-rust check-swift audit

# Explicit alias for callers that distinguish Rust-only and full checks.
check-all: check

# Remove Rust and Swift build artifacts.
clean:
    cargo clean
    swift package --package-path {{app_dir}} clean
