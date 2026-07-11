#!/bin/bash
set -euo pipefail

app_dir="$(cd "$(dirname "$0")" && pwd)"
repo_dir="$(cd "$app_dir/../.." && pwd)"
build_dir="$app_dir/.build"
configuration="${1:-debug}"
cargo_args=(build)

case "$configuration" in
  debug)
    app="$build_dir/UsageMenuBar-dev.app"
    cargo_profile="debug"
    ;;
  release)
    app="$build_dir/UsageMenuBar.app"
    cargo_profile="release"
    cargo_args+=(--release)
    ;;
  *)
    echo "Usage: $0 [debug|release]" >&2
    exit 2
    ;;
esac

swift build -c "$configuration" --package-path "$app_dir"
CARGO_TARGET_DIR="$repo_dir/target" \
  cargo "${cargo_args[@]}" --manifest-path "$repo_dir/Cargo.toml" -p usage-daemon

swift_bin_dir="$(swift build -c "$configuration" --package-path "$app_dir" --show-bin-path)"

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$app_dir/Info.plist" "$app/Contents/Info.plist"
cp "$swift_bin_dir/UsageMenuBar" "$app/Contents/MacOS/UsageMenuBar"
cp "$repo_dir/target/$cargo_profile/usage-daemon" "$app/Contents/MacOS/usage-daemon"
cp -R "$swift_bin_dir/UsageMenuBar_UsageMenuBar.bundle" "$app/Contents/Resources/"

codesign --force --sign - --identifier engineering.super.usagetracker.daemon \
  "$app/Contents/MacOS/usage-daemon"
codesign --force --sign - "$app"

echo "Built $configuration app at $app"
