#!/bin/sh
set -eu

export RUST_TEST_THREADS=1

cargo test --release -p usage-core benchmark_core_response_pipeline -- --ignored --nocapture
cargo test --release -p usage-daemon benchmark_daemon_response_pipeline -- --ignored --nocapture
cargo test --release -p usage-cli benchmark_cli_response_pipeline -- --ignored --nocapture
