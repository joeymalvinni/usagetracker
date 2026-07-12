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
    bundle_identifier="engineering.super.usagetracker.dev"
    # Launch Services and Notification Center cache icons by bundle identity
    # and version. Give each fixture build a new version so it cannot retain a
    # low-resolution icon from an earlier package.
    bundle_version="$(date +%Y%m%d%H%M%S)"
    ;;
  release)
    app="$build_dir/UsageMenuBar.app"
    cargo_profile="release"
    bundle_identifier="engineering.super.usagetracker"
    bundle_version="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleVersion' "$app_dir/Info.plist")"
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
/usr/libexec/PlistBuddy -c "Set :CFBundleIdentifier $bundle_identifier" "$app/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleVersion $bundle_version" "$app/Contents/Info.plist"
cp "$swift_bin_dir/UsageMenuBar" "$app/Contents/MacOS/UsageMenuBar"
cp "$repo_dir/target/$cargo_profile/usage-daemon" "$app/Contents/MacOS/usage-daemon"
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

codesign --force --sign - --identifier "$bundle_identifier.daemon" \
  "$app/Contents/MacOS/usage-daemon"
codesign --force --sign - "$app"

echo "Built $configuration app at $app"
