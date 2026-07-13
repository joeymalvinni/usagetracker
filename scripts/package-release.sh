#!/bin/bash
set -euo pipefail

script_dir="$(cd "$(dirname "$0")" && pwd)"
repo_dir="$(cd "$script_dir/.." && pwd)"

usage() {
  cat <<'EOF'
Usage: scripts/package-release.sh <aarch64-apple-darwin|x86_64-apple-darwin> <output-directory>

Builds one signed, architecture-specific macOS app and CLI release. Set
CODESIGN_IDENTITY to a Developer ID Application identity. To notarize, either
set NOTARY_KEYCHAIN_PROFILE or set APPLE_ID, APPLE_TEAM_ID, and
APPLE_APP_SPECIFIC_PASSWORD.
EOF
}

if [[ $# -ne 2 ]]; then
  usage >&2
  exit 2
fi

target_triple="$1"
output_dir="$2"
case "$target_triple" in
  aarch64-apple-darwin)
    release_arch="arm64"
    ;;
  x86_64-apple-darwin)
    release_arch="x86_64"
    ;;
  *)
    usage >&2
    exit 2
    ;;
esac

workspace_version="$({
  awk '
    /^\[workspace\.package\]$/ { in_workspace_package = 1; next }
    /^\[/ { in_workspace_package = 0 }
    in_workspace_package && /^version = / {
      gsub(/^[^\"]*\"|\".*$/, "")
      print
      exit
    }
  ' "$repo_dir/Cargo.toml"
} || true)"
version="${RELEASE_VERSION:-$workspace_version}"
version="${version#v}"
if [[ -z "$version" ]]; then
  echo "Could not determine the release version" >&2
  exit 1
fi

if [[ "${REQUIRE_SIGNING:-0}" == "1" && "${CODESIGN_IDENTITY:--}" == "-" ]]; then
  echo "A Developer ID Application CODESIGN_IDENTITY is required" >&2
  exit 1
fi

mkdir -p "$output_dir"
output_dir="$(cd "$output_dir" && pwd)"
work_dir="$(mktemp -d "${TMPDIR:-/tmp}/usagetracker-release.XXXXXX")"
trap 'rm -rf "$work_dir"' EXIT

app_path="$work_dir/UsageTracker.app"
export APP_OUTPUT_PATH="$app_path"
export BUNDLE_SHORT_VERSION="$version"
export BUNDLE_VERSION="${BUNDLE_VERSION:-${GITHUB_RUN_NUMBER:-1}}"
export CARGO_LOCKED=1
export USAGE_TARGET_TRIPLE="$target_triple"

rustup target add "$target_triple"
"$repo_dir/apps/UsageMenuBar/package-dev-app.sh" release

CARGO_TARGET_DIR="$repo_dir/target" cargo build \
  --manifest-path "$repo_dir/Cargo.toml" \
  --release \
  --locked \
  --target "$target_triple" \
  -p usage-cli

cli_path="$work_dir/usage"
cp "$repo_dir/target/$target_triple/release/usage-cli" "$cli_path"
chmod 0755 "$cli_path"
app_identifier="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$app_path/Contents/Info.plist")"
cli_identifier="$app_identifier.cli"

if [[ "${CODESIGN_IDENTITY:--}" == "-" ]]; then
  codesign --force --sign - --identifier "$cli_identifier" "$cli_path"
else
  codesign --force --timestamp --options runtime --sign "$CODESIGN_IDENTITY" \
    --identifier "$cli_identifier" "$cli_path"
fi
codesign --verify --strict --verbose=2 "$cli_path"

notary_payload="$work_dir/notarization"
notary_archive="$work_dir/notarization.zip"
mkdir -p "$notary_payload"
ditto "$app_path" "$notary_payload/UsageTracker.app"
cp "$cli_path" "$notary_payload/usage"
ditto -c -k "$notary_payload" "$notary_archive"

notarized=0
if [[ -n "${NOTARY_KEYCHAIN_PROFILE:-}" ]]; then
  xcrun notarytool submit "$notary_archive" \
    --keychain-profile "$NOTARY_KEYCHAIN_PROFILE" \
    --wait
  notarized=1
elif [[ -n "${APPLE_ID:-}" && -n "${APPLE_TEAM_ID:-}" && -n "${APPLE_APP_SPECIFIC_PASSWORD:-}" ]]; then
  xcrun notarytool submit "$notary_archive" \
    --apple-id "$APPLE_ID" \
    --team-id "$APPLE_TEAM_ID" \
    --password "$APPLE_APP_SPECIFIC_PASSWORD" \
    --wait
  notarized=1
elif [[ "${REQUIRE_NOTARIZATION:-0}" == "1" ]]; then
  echo "Notarization credentials are required" >&2
  exit 1
fi

if [[ "$notarized" == "1" ]]; then
  xcrun stapler staple "$app_path"
  xcrun stapler validate "$app_path"
fi

codesign --verify --deep --strict --verbose=2 "$app_path"

app_archive="$output_dir/UsageTracker-macos-$release_arch.zip"
cli_archive="$output_dir/usage-macos-$release_arch.tar.gz"
rm -f "$app_archive" "$cli_archive"
ditto -c -k --keepParent "$app_path" "$app_archive"
COPYFILE_DISABLE=1 tar -czf "$cli_archive" -C "$work_dir" usage

echo "Created $app_archive"
echo "Created $cli_archive"
