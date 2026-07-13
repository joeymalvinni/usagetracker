#!/bin/bash
set -euo pipefail

repo_dir="$(cd "$(dirname "$0")/.." && pwd)"
tag="${1:-${GITHUB_REF_NAME:-}}"
if [[ -z "$tag" ]]; then
  echo "Usage: scripts/check-release-version.sh <tag>" >&2
  exit 2
fi

version="${tag#v}"
if [[ "$tag" != "v$version" || ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "Release tags must have the form vMAJOR.MINOR.PATCH; got $tag" >&2
  exit 1
fi

cargo_version="$({
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
plist_version="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' \
  "$repo_dir/apps/UsageMenuBar/Info.plist")"

if [[ "$cargo_version" != "$version" ]]; then
  echo "Cargo workspace version is $cargo_version, but the release tag is $tag" >&2
  exit 1
fi
if [[ "$plist_version" != "$version" ]]; then
  echo "App version is $plist_version, but the release tag is $tag" >&2
  exit 1
fi
if [[ ! -f "$repo_dir/docs/releases/$tag.md" ]]; then
  echo "Release notes are missing: docs/releases/$tag.md" >&2
  exit 1
fi

echo "Release version and notes agree: $version"
