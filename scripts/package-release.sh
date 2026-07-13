#!/bin/bash
set -euo pipefail

script_dir="$(cd "$(dirname "$0")" && pwd)"
repo_dir="$(cd "$script_dir/.." && pwd)"

usage() {
  cat <<'EOF'
Usage: scripts/package-release.sh <aarch64-apple-darwin|x86_64-apple-darwin> <output-directory>

Builds one architecture-specific macOS app and CLI release with ad-hoc code
signatures. The resulting artifacts are not notarized by Apple.
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

mkdir -p "$output_dir"
output_dir="$(cd "$output_dir" && pwd)"
work_dir="$(mktemp -d "${TMPDIR:-/tmp}/usagetracker-release.XXXXXX")"
trap 'rm -rf "$work_dir"' EXIT

app_path="$work_dir/UsageTracker.app"
export APP_OUTPUT_PATH="$app_path"
export BUNDLE_SHORT_VERSION="$version"
export BUNDLE_VERSION="${BUNDLE_VERSION:-${GITHUB_RUN_NUMBER:-1}}"
export CARGO_LOCKED=1
export CODESIGN_IDENTITY="-"
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

codesign --force --sign - --identifier "$cli_identifier" "$cli_path"
codesign --verify --strict --verbose=2 "$cli_path"
codesign --verify --deep --strict --verbose=2 "$app_path"

app_archive="$output_dir/UsageTracker-macos-$release_arch.zip"
cli_archive="$output_dir/usage-macos-$release_arch.tar.gz"
rm -f "$app_archive" "$cli_archive"
ditto -c -k --keepParent "$app_path" "$app_archive"
COPYFILE_DISABLE=1 tar -czf "$cli_archive" -C "$work_dir" usage

echo "Created $app_archive"
echo "Created $cli_archive"
