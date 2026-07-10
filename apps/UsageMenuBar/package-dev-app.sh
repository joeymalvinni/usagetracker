#!/bin/bash
set -euo pipefail

app_dir="$(cd "$(dirname "$0")" && pwd)"
repo_dir="$(cd "$app_dir/../.." && pwd)"
build_dir="$app_dir/.build"
app="$build_dir/UsageMenuBar.app"

swift build --package-path "$app_dir"
CARGO_TARGET_DIR="$repo_dir/target" \
  cargo build --manifest-path "$repo_dir/Cargo.toml" -p usage-daemon

swift_bin_dir="$(swift build --package-path "$app_dir" --show-bin-path)"

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$app_dir/Info.plist" "$app/Contents/Info.plist"
cp "$swift_bin_dir/UsageMenuBar" "$app/Contents/MacOS/UsageMenuBar"
cp "$repo_dir/target/debug/usage-daemon" "$app/Contents/MacOS/usage-daemon"
cp -R "$swift_bin_dir/UsageMenuBar_UsageMenuBar.bundle" "$app/Contents/Resources/"

codesign --force --sign - --identifier engineering.super.usagetracker.daemon \
  "$app/Contents/MacOS/usage-daemon"
codesign --force --sign - "$app"

echo "Built $app"
