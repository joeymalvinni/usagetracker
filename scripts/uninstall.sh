#!/bin/bash
set -euo pipefail

app_dir="$HOME/Applications"
bin_dir="$HOME/.local/bin"
purge_data=0
force=0
daemon_home="${USAGE_TRACKER_HOME:-$HOME/.usagetracker}"
daemon_socket="${USAGE_TRACKER_SOCKET:-$daemon_home/usage.sock}"
launch_agent_label="app.usagetracker.daemon"

usage() {
  cat <<'EOF'
Usage: uninstall.sh [options]

Options:
  --app-dir DIR   App directory used during installation
  --bin-dir DIR   CLI directory used during installation
  --purge-data    Also remove USAGE_TRACKER_HOME (default: ~/.usagetracker; irreversible)
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

wait_for_processes() {
  local attempts="$1" attempt pid running
  shift
  for ((attempt = 0; attempt < attempts; attempt++)); do
    running=0
    for pid in "$@"; do
      if kill -0 "$pid" 2>/dev/null; then running=1; fi
    done
    [[ "$running" == "0" ]] && return 0
    sleep 0.25
  done
  return 1
}

unregister_launch_agent() {
  local executable="$app_path/Contents/MacOS/UsageMenuBar"
  if [[ -x "$executable" ]]; then
    "$executable" --unregister-daemon-agent >/dev/null 2>&1 || return 1
  fi
  launchctl bootout "gui/$(id -u)/$launch_agent_label" >/dev/null 2>&1 || true
}

stop_daemons() {
  local source pid executable command_line
  local pids=()
  while read -r source pid; do
    [[ -n "$pid" ]] || continue
    executable="$(ps -p "$pid" -o comm= 2>/dev/null || true)"
    executable="${executable#${executable%%[![:space:]]*}}"
    [[ "${executable##*/}" == "usage-daemon" ]] || continue
    if [[ "$source" == "lock" ]]; then
      pids+=("$pid")
      continue
    fi
    [[ "$executable" == "$app_path/Contents/MacOS/usage-daemon" ]] || continue
    command_line="$(ps -ww -p "$pid" -o command= 2>/dev/null || true)"
    case "$command_line" in
      *" --socket-path $daemon_socket"|*" --socket-path $daemon_socket "*) pids+=("$pid") ;;
    esac
  done < <({
    /usr/sbin/lsof -t -- "$daemon_socket.lock" 2>/dev/null | awk '{ print "lock " $1 }' || true
    pgrep -x usage-daemon 2>/dev/null | awk '{ print "scan " $1 }' || true
  } | sort -u -k2,2)
  [[ "${#pids[@]}" -gt 0 ]] || return 0

  kill -TERM "${pids[@]}" 2>/dev/null || true
  if wait_for_processes 20 "${pids[@]}"; then return 0; fi
  for pid in "${pids[@]}"; do
    if kill -0 "$pid" 2>/dev/null; then kill -KILL "$pid" 2>/dev/null || true; fi
  done
  wait_for_processes 20 "${pids[@]}"
}

if [[ -d "$app_path" ]]; then
  bundle_id="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$app_path/Contents/Info.plist" 2>/dev/null || true)"
  app_signature="$(signature_field "$app_path" Signature || true)"
  if [[ -f "$app_receipt" ]]; then
    expected_bundle_id="$(sed -n '1p' "$app_receipt")"
    expected_app_signature="$(sed -n '2p' "$app_receipt")"
  else
    expected_bundle_id=""
    expected_app_signature=""
  fi
  if [[ "$force" != "1" && \
        ( -z "$expected_bundle_id" || "$bundle_id" != "$expected_bundle_id" || "$app_signature" != "$expected_app_signature" ) ]]; then
    echo "Refusing to remove an app without a matching UsageTracker install receipt: $app_path" >&2
    echo "Pass --force to override." >&2
    exit 1
  fi
fi

if [[ -f "$cli_path" ]]; then
  cli_id="$(signature_field "$cli_path" Identifier || true)"
  cli_signature="$(signature_field "$cli_path" Signature || true)"
  if [[ -f "$cli_receipt" ]]; then
    expected_cli_id="$(sed -n '1p' "$cli_receipt")"
    expected_cli_signature="$(sed -n '2p' "$cli_receipt")"
  else
    expected_cli_id=""
    expected_cli_signature=""
  fi
  if [[ "$force" != "1" && \
        ( -z "$expected_cli_id" || "$cli_id" != "$expected_cli_id" || "$cli_signature" != "$expected_cli_signature" ) ]]; then
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

if ! unregister_launch_agent; then
  echo "UsageTracker background service could not be unregistered." >&2
  exit 1
fi
if ! stop_daemons; then
  echo "UsageTracker background service could not be stopped." >&2
  exit 1
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
  data_dir="$daemon_home"
  rm -rf "$data_dir"
  echo "Removed $data_dir"
else
  echo "Kept $daemon_home data. Pass --purge-data to remove it."
fi
