#!/bin/bash
set -euo pipefail

app_dir="$(cd "$(dirname "$0")" && pwd)"
repo_dir="$(cd "$app_dir/../.." && pwd)"
build_dir="$app_dir/.build"
configuration="${1:-debug}"
cargo_args=(build)
target_triple="${USAGE_TARGET_TRIPLE:-}"
codesign_identity="${CODESIGN_IDENTITY:--}"
swift_args=(swift build -c "$configuration" --package-path "$app_dir")
base_bundle_identifier="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$app_dir/Info.plist")"

if [[ "${CARGO_LOCKED:-0}" == "1" ]]; then
  cargo_args+=(--locked)
fi

if [[ -n "$target_triple" ]]; then
  case "$target_triple" in
    aarch64-apple-darwin)
      swift_triple="arm64-apple-macosx14.0"
      ;;
    x86_64-apple-darwin)
      swift_triple="x86_64-apple-macosx14.0"
      ;;
    *)
      echo "Unsupported release target: $target_triple" >&2
      exit 2
      ;;
  esac
  cargo_args+=(--target "$target_triple")
  swift_args+=(--triple "$swift_triple")
fi

case "$configuration" in
  debug)
    app="$build_dir/UsageMenuBar-dev.app"
    cargo_profile="debug"
    bundle_identifier="$base_bundle_identifier.dev"
    # Launch Services and Notification Center cache icons by bundle identity
    # and version. Give each fixture build a new version so it cannot retain a
    # low-resolution icon from an earlier package.
    bundle_version="$(date +%Y%m%d%H%M%S)"
    ;;
  release)
    app="${APP_OUTPUT_PATH:-$build_dir/UsageMenuBar.app}"
    cargo_profile="release"
    bundle_identifier="$base_bundle_identifier"
    bundle_version="${BUNDLE_VERSION:-$(/usr/libexec/PlistBuddy -c 'Print :CFBundleVersion' "$app_dir/Info.plist")}"
    cargo_args+=(--release)
    ;;
  *)
    echo "Usage: $0 [debug|release]" >&2
    exit 2
    ;;
esac

bundle_short_version="${BUNDLE_SHORT_VERSION:-$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$app_dir/Info.plist")}"

"${swift_args[@]}"
CARGO_TARGET_DIR="$repo_dir/target" \
  cargo "${cargo_args[@]}" --manifest-path "$repo_dir/Cargo.toml" -p usage-daemon

swift_bin_dir="$("${swift_args[@]}" --show-bin-path)"
if [[ -n "$target_triple" ]]; then
  cargo_bin_dir="$repo_dir/target/$target_triple/$cargo_profile"
else
  cargo_bin_dir="$repo_dir/target/$cargo_profile"
fi

rm -rf "$app"
mkdir -p \
  "$app/Contents/MacOS" \
  "$app/Contents/Resources" \
  "$app/Contents/Library/LaunchAgents"
cp "$app_dir/Info.plist" "$app/Contents/Info.plist"
cp "$app_dir/LaunchAgents/engineering.super.usagetracker.daemon.plist" \
  "$app/Contents/Library/LaunchAgents/"
/usr/libexec/PlistBuddy -c "Set :CFBundleIdentifier $bundle_identifier" "$app/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleVersion $bundle_version" "$app/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $bundle_short_version" "$app/Contents/Info.plist"
cp "$swift_bin_dir/UsageMenuBar" "$app/Contents/MacOS/UsageMenuBar"
cp "$cargo_bin_dir/usage-daemon" "$app/Contents/MacOS/usage-daemon"
cp -R "$swift_bin_dir/UsageMenuBar_UsageMenuBar.bundle" "$app/Contents/Resources/"
xcrun actool "$app_dir/AppIcon.icon" \
  --compile "$app/Contents/Resources" \
  --platform macosx \
  --minimum-deployment-target 14.0 \
  --app-icon AppIcon \
  --output-partial-info-plist "$build_dir/AppIcon-info.plist" \
  --output-format human-readable-text \
  --warnings \
  --notices

# Icon Composer puts adaptive appearances in Assets.car, but actool's legacy
# AppIcon.icns fallback currently stops at 256 px. Notification Center reads
# that fallback for banners, making the icon visibly soft. Replace it with a
# complete dark-mode iconset rendered from the vector source. The asset catalog
# also uses a dark background for every appearance because notification banners
# follow the system appearance, not Usage's in-app appearance preference.
icon_composer_tool="$(dirname "$(xcode-select -p)")/Applications/Icon Composer.app/Contents/Executables/ictool"
iconset_dir="$build_dir/AppIcon.iconset"
icon_source="$build_dir/AppIcon-dark-1024.png"
rm -rf "$iconset_dir"
mkdir -p "$iconset_dir"
"$icon_composer_tool" "$app_dir/AppIcon.icon" \
  --export-image \
  --output-file "$icon_source" \
  --platform macOS \
  --rendition Dark \
  --width 1024 \
  --height 1024 \
  --scale 1 >/dev/null

render_icon_size() {
  local pixels="$1"
  local filename="$2"
  sips -z "$pixels" "$pixels" "$icon_source" \
    --out "$iconset_dir/$filename" >/dev/null
}

render_icon_size 16 icon_16x16.png
render_icon_size 32 icon_16x16@2x.png
cp "$iconset_dir/icon_16x16@2x.png" "$iconset_dir/icon_32x32.png"
render_icon_size 64 icon_32x32@2x.png
render_icon_size 128 icon_128x128.png
render_icon_size 256 icon_128x128@2x.png
cp "$iconset_dir/icon_128x128@2x.png" "$iconset_dir/icon_256x256.png"
render_icon_size 512 icon_256x256@2x.png
cp "$iconset_dir/icon_256x256@2x.png" "$iconset_dir/icon_512x512.png"
cp "$icon_source" "$iconset_dir/icon_512x512@2x.png"
iconutil -c icns "$iconset_dir" -o "$app/Contents/Resources/AppIcon.icns"
rm -rf "$iconset_dir" "$icon_source"

if [[ "$codesign_identity" == "-" ]]; then
  codesign --force --sign - --identifier "$bundle_identifier.daemon" \
    "$app/Contents/MacOS/usage-daemon"
  codesign --force --sign - "$app"
else
  codesign --force --timestamp --options runtime --sign "$codesign_identity" \
    --identifier "$bundle_identifier.daemon" "$app/Contents/MacOS/usage-daemon"
  codesign --force --timestamp --options runtime --sign "$codesign_identity" "$app"
fi

codesign --verify --deep --strict --verbose=2 "$app"

echo "Built $configuration app at $app"
