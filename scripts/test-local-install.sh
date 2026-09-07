#!/bin/bash
set -euo pipefail

script_dir="$(cd "$(dirname "$0")" && pwd)"
repo_dir="$(cd "$script_dir/.." && pwd)"
app_dir="$repo_dir/apps/UsageMenuBar"
app_bundle="$app_dir/.build/UsageMenuBar-dev.app"
app_executable="$app_bundle/Contents/MacOS/UsageMenuBar"
temporary_parent="${TMPDIR:-/tmp}"
test_root="$(mktemp -d "$temporary_parent/usagetracker-local-install.XXXXXX")"
test_home="$test_root/home"

cleanup() {
  local status=$?
  case "$test_root" in
    "$temporary_parent"/usagetracker-local-install.*)
      rm -rf "$test_root"
      ;;
    *)
      echo "Refusing to remove unexpected test directory: $test_root" >&2
      ;;
  esac
  return "$status"
}
trap cleanup EXIT

mkdir -p "$test_home"

printf '\nUsageTracker local install test\n\n'
printf '  Building current working tree...\n'
"$app_dir/package-dev-app.sh" debug

if [[ ! -x "$app_executable" ]]; then
  echo "Local app executable was not created at $app_executable" >&2
  exit 1
fi

printf '\n'
printf '  App   %s\n' "$app_bundle"
printf '  Data  %s (temporary)\n' "$test_home"
printf '\n'
printf 'Launching with clean first-run state.\n'
printf 'Quit from the menu-bar context menu or press Control-C to stop.\n\n'

USAGE_TRACKER_HOME="$test_home" "$app_executable"
