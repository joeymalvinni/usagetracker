#!/bin/bash
set -euo pipefail

script_dir="$(cd "$(dirname "$0")" && pwd)"
repo_dir="$(cd "$script_dir/.." && pwd)"

usage() {
  cat <<'EOF'
Usage: scripts/package-release.sh <aarch64-apple-darwin|x86_64-apple-darwin> <output-directory>

Builds one architecture-specific macOS app and CLI release.

Defaults to ad-hoc signing for local builds. For a notarized release, set
CODESIGN_IDENTITY to a Developer ID Application identity and NOTARYTOOL_PROFILE
to stored notarytool credentials. NOTARYTOOL_KEYCHAIN is optional.
EOF
}

if [[ $# -ne 2 ]]; then
  usage >&2
  exit 2
fi

target_triple="$1"
output_dir="$2"
codesign_identity="${CODESIGN_IDENTITY:--}"
if [[ "$codesign_identity" != "-" && -z "${NOTARYTOOL_PROFILE:-}" ]]; then
  echo "Developer ID releases require NOTARYTOOL_PROFILE for notarization" >&2
  exit 2
fi
if [[ "$codesign_identity" == "-" && -n "${NOTARYTOOL_PROFILE:-}" ]]; then
  echo "Notarization requires a Developer ID Application signing identity" >&2
  exit 2
fi
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

mkdir -p "$output_dir"
output_dir="$(cd "$output_dir" && pwd)"
work_dir="$(mktemp -d "${TMPDIR:-/tmp}/usagetracker-release.XXXXXX")"
trap 'rm -rf "$work_dir"' EXIT

app_path="$work_dir/UsageTracker.app"
export APP_OUTPUT_PATH="$app_path"
export BUNDLE_SHORT_VERSION="$version"
export BUNDLE_VERSION="${BUNDLE_VERSION:-${GITHUB_RUN_NUMBER:-1}}"
export CARGO_LOCKED=1
export CODESIGN_IDENTITY="$codesign_identity"
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

if [[ "$codesign_identity" == "-" ]]; then
  codesign --force --sign - --identifier "$cli_identifier" "$cli_path"
else
  codesign --force --timestamp --options runtime --sign "$codesign_identity" \
    --identifier "$cli_identifier" "$cli_path"
fi
codesign --verify --strict --verbose=2 "$cli_path"
codesign --verify --deep --strict --verbose=2 "$app_path"

if [[ "$codesign_identity" != "-" ]]; then
  # Submit both binaries together. Staple the app before making the download
  # archive; standalone CLI executables cannot carry a stapled ticket.
  submission_dir="$work_dir/submission"
  mkdir -p "$submission_dir"
  ditto "$app_path" "$submission_dir/UsageTracker.app"
  cp "$cli_path" "$submission_dir/usage"
  ditto -c -k "$submission_dir" "$work_dir/notarization.zip"
  notary_args=(--keychain-profile "$NOTARYTOOL_PROFILE")
  if [[ -n "${NOTARYTOOL_KEYCHAIN:-}" ]]; then
    notary_args+=(--keychain "$NOTARYTOOL_KEYCHAIN")
  fi
  if ! xcrun notarytool submit "$work_dir/notarization.zip" "${notary_args[@]}" \
    --wait --timeout 30m --output-format json > "$work_dir/notarization.json"; then
    cat "$work_dir/notarization.json" >&2
    exit 1
  fi
  python3 - "$work_dir/notarization.json" <<'PY'
import json
import sys

with open(sys.argv[1]) as result_file:
    result = json.load(result_file)
if result.get("status") != "Accepted":
    raise SystemExit(f"Notarization was not accepted (submission {result.get('id', 'unknown')})")
PY
  xcrun stapler staple "$app_path"
  xcrun stapler validate "$app_path"
  spctl --assess --type execute --verbose=2 "$app_path"
fi

app_archive="$output_dir/UsageTracker-macos-$release_arch.zip"
cli_archive="$output_dir/usage-macos-$release_arch.tar.gz"
rm -f "$app_archive" "$cli_archive"
ditto -c -k --keepParent "$app_path" "$app_archive"
COPYFILE_DISABLE=1 tar -czf "$cli_archive" -C "$work_dir" usage

echo "Created $app_archive"
echo "Created $cli_archive"
