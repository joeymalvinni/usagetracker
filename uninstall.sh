#!/bin/bash
set -euo pipefail

app_dir="$HOME/Applications"
bin_dir="$HOME/.local/bin"
purge_data=0
force=0

usage() {
  cat <<'EOF'
Usage: uninstall.sh [options]

Options:
  --app-dir DIR   App directory used during installation
  --bin-dir DIR   CLI directory used during installation
  --purge-data    Also remove ~/.usagetracker (irreversible)
  --force         Remove destination files even if no matching install receipt exists
  -h, --help      Show this help
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --app-dir)
      [[ $# -ge 2 ]] || { echo "--app-dir requires a value" >&2; exit 2; }
      app_dir="$2"
      shift 2
      ;;
    --bin-dir)
      [[ $# -ge 2 ]] || { echo "--bin-dir requires a value" >&2; exit 2; }
      bin_dir="$2"
      shift 2
      ;;
    --purge-data)
      purge_data=1
      shift
      ;;
    --force)
      force=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "Unknown option: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

app_path="$app_dir/UsageTracker.app"
cli_path="$bin_dir/usage"
app_receipt="$app_dir/.UsageTracker.app.install-receipt"
cli_receipt="$bin_dir/.usage.install-receipt"

signature_field() {
  local path="$1"
  local field="$2"
  codesign -dv --verbose=4 "$path" 2>&1 | \
    awk -F= -v field="$field" '$1 == field { print $2; exit }'
}

if [[ -d "$app_path" ]]; then
  bundle_id="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$app_path/Contents/Info.plist" 2>/dev/null || true)"
  app_team="$(signature_field "$app_path" TeamIdentifier || true)"
  if [[ -f "$app_receipt" ]]; then
    expected_bundle_id="$(sed -n '1p' "$app_receipt")"
    expected_app_team="$(sed -n '2p' "$app_receipt")"
  else
    expected_bundle_id=""
    expected_app_team=""
  fi
  if [[ "$force" != "1" && \
        ( -z "$expected_bundle_id" || "$bundle_id" != "$expected_bundle_id" || "$app_team" != "$expected_app_team" ) ]]; then
    echo "Refusing to remove an app without a matching UsageTracker install receipt: $app_path" >&2
    echo "Pass --force to override." >&2
    exit 1
  fi
fi

if [[ -f "$cli_path" ]]; then
  cli_id="$(signature_field "$cli_path" Identifier || true)"
  cli_team="$(signature_field "$cli_path" TeamIdentifier || true)"
  if [[ -f "$cli_receipt" ]]; then
    expected_cli_id="$(sed -n '1p' "$cli_receipt")"
    expected_cli_team="$(sed -n '2p' "$cli_receipt")"
  else
    expected_cli_id=""
    expected_cli_team=""
  fi
  if [[ "$force" != "1" && \
        ( -z "$expected_cli_id" || "$cli_id" != "$expected_cli_id" || "$cli_team" != "$expected_cli_team" ) ]]; then
    echo "Refusing to remove a command without a matching UsageTracker install receipt: $cli_path" >&2
    echo "Pass --force to override." >&2
    exit 1
  fi
fi

if pgrep -x UsageMenuBar >/dev/null 2>&1; then
  if [[ -n "${bundle_id:-}" ]]; then
    osascript -e "tell application id \"$bundle_id\" to quit" >/dev/null 2>&1 || true
  fi
fi

if [[ -d "$app_path" ]]; then
  rm -rf "$app_path"
  rm -f "$app_receipt"
  echo "Removed $app_path"
elif [[ -f "$app_receipt" ]]; then
  rm -f "$app_receipt"
fi

if [[ -f "$cli_path" ]]; then
  rm -f "$cli_path"
  rm -f "$cli_receipt"
  echo "Removed $cli_path"
elif [[ -f "$cli_receipt" ]]; then
  rm -f "$cli_receipt"
fi

if [[ "$purge_data" == "1" ]]; then
  data_dir="$HOME/.usagetracker"
  rm -rf "$data_dir"
  echo "Removed $data_dir"
else
  echo "Kept ~/.usagetracker data. Pass --purge-data to remove it."
fi
