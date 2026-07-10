set shell := ["zsh", "-cu"]

app_dir := "apps/UsageMenuBar"
app_bundle := app_dir + "/.build/UsageMenuBar.app"

# Show the available commands.
default:
    @just --list

# Build the Rust workspace and the signed macOS app bundle.
build: build-rust build-app

# Build all Rust crates.
build-rust:
    cargo build

# Build and ad-hoc sign the macOS app bundle, including the daemon.
build-app:
    ./{{app_dir}}/package-dev-app.sh

# Build and launch the signed macOS app bundle.
app: build-app
    open -n {{app_bundle}}

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
    cargo clippy --all-targets

# Format Rust sources.
fmt:
    cargo fmt --all

# Verify Rust formatting without changing files.
fmt-check:
    cargo fmt --all -- --check

# Run the full local verification suite.
check: fmt-check clippy test

# Remove Rust and Swift build artifacts.
clean:
    cargo clean
    swift package --package-path {{app_dir}} clean
