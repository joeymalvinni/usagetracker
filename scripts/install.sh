#!/bin/bash
set -euo pipefail

repository="${USAGETRACKER_REPOSITORY:-joeymalvinni/usagetracker}"
version=""
app_dir="$HOME/Applications"
bin_dir="$HOME/.local/bin"
install_app=1
install_cli=1
launch_app=1
force=0
app_was_running=0
update_status_file="${USAGETRACKER_UPDATE_STATUS_FILE:-}"
daemon_home="${USAGE_TRACKER_HOME:-$HOME/.usagetracker}"
daemon_socket="${USAGE_TRACKER_SOCKET:-$daemon_home/usage.sock}"

if [[ -t 1 && -z "${NO_COLOR:-}" && "${TERM:-}" != "dumb" ]]; then
  style_bold=$'\033[1m'
  style_dim=$'\033[2m'
  style_cyan=$'\033[36m'
  style_green=$'\033[32m'
  style_yellow=$'\033[33m'
  style_reset=$'\033[0m'
else
  style_bold=""
  style_dim=""
  style_cyan=""
  style_green=""
  style_yellow=""
  style_reset=""
fi

status() {
  printf '  %s→%s %s\n' "$style_cyan" "$style_reset" "$1"
}

success() {
  printf '  %s✓%s %s\n' "$style_green" "$style_reset" "$1"
}

notice() {
  printf '  %s!%s %s\n' "$style_yellow" "$style_reset" "$1"
}

usage() {
  cat <<'EOF'
Install UsageTracker from a checksum-verified GitHub release.

Release binaries have ad-hoc code signatures and are not notarized by Apple.
macOS may require manual approval the first time the app opens.

Usage: install.sh [options]

Options:
  --version VERSION  Install a specific release, such as v0.1.0
  --app-only         Install only UsageTracker.app
  --cli-only         Install only the usage command
  --app-dir DIR      App destination directory (default: ~/Applications)
  --bin-dir DIR      CLI destination directory (default: ~/.local/bin)
  --no-launch        Do not launch the app after installation
  --force            Replace files at the destinations even if they are not UsageTracker
  -h, --help         Show this help

Examples:
  curl -fsSL https://github.com/joeymalvinni/usagetracker/releases/latest/download/install.sh | bash
  curl -fsSL https://github.com/joeymalvinni/usagetracker/releases/latest/download/install.sh | bash -s -- --cli-only
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --version)
      if [[ $# -lt 2 ]]; then
        echo "--version requires a value" >&2
        exit 2
      fi
      version="$2"
      shift 2
      ;;
    --app-only)
      install_cli=0
      shift
      ;;
    --cli-only)
      install_app=0
      launch_app=0
      shift
      ;;
    --app-dir)
      if [[ $# -lt 2 ]]; then
        echo "--app-dir requires a value" >&2
        exit 2
      fi
      app_dir="$2"
      shift 2
      ;;
    --bin-dir)
      if [[ $# -lt 2 ]]; then
        echo "--bin-dir requires a value" >&2
        exit 2
      fi
      bin_dir="$2"
      shift 2
      ;;
    --no-launch)
      launch_app=0
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

if [[ "$install_app" == "0" && "$install_cli" == "0" ]]; then
  echo "--app-only and --cli-only cannot be used together" >&2
  exit 2
fi
if [[ -n "$version" ]]; then
  version="${version#v}"
  if [[ ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "Version must have the form vMAJOR.MINOR.PATCH or MAJOR.MINOR.PATCH" >&2
    exit 2
  fi
  version="v$version"
fi

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "UsageTracker currently requires macOS" >&2
  exit 1
fi

macos_version="$(sw_vers -productVersion)"
macos_major="${macos_version%%.*}"
if [[ ! "$macos_major" =~ ^[0-9]+$ || "$macos_major" -lt 14 ]]; then
  echo "UsageTracker requires macOS 14 or newer; found $macos_version" >&2
  exit 1
fi

machine="$(uname -m)"
if [[ "$machine" == "x86_64" && "$(sysctl -in sysctl.proc_translated 2>/dev/null || true)" == "1" ]]; then
  machine="arm64"
fi
case "$machine" in
  arm64)
    release_arch="arm64"
    architecture_name="Apple silicon"
    ;;
  x86_64)
    release_arch="x86_64"
    architecture_name="Intel"
    ;;
  *)
    echo "Unsupported Mac architecture: $machine" >&2
    exit 1
    ;;
esac

for command in curl ditto shasum tar unzip; do
  if ! command -v "$command" >/dev/null 2>&1; then
    echo "Required command not found: $command" >&2
    exit 1
  fi
done

if [[ -n "$version" ]]; then
  download_base="https://github.com/$repository/releases/download/$version"
else
  download_base="https://github.com/$repository/releases/latest/download"
fi

work_dir="$(mktemp -d "${TMPDIR:-/tmp}/usagetracker-install.XXXXXX")"
new_app=""
backup_app=""
cleanup() {
  local status=$?
  set +e
  if [[ "$status" != "0" && -n "$update_status_file" ]]; then
    printf '%s\n' "$status" > "$update_status_file" 2>/dev/null || true
  fi
  rm -rf "$work_dir"
  if [[ -n "$new_app" ]]; then rm -rf "$new_app"; fi
  if [[ -n "$backup_app" && -e "$backup_app" ]]; then
    echo "A previous app was preserved at $backup_app" >&2
  fi
  if [[ "$status" != "0" && "$app_was_running" == "1" && -d "${app_path:-}" ]]; then
    open "$app_path" >/dev/null 2>&1 || true
  fi
  return "$status"
}
trap cleanup EXIT

printf '\n%sUsageTracker%s\n' "$style_bold" "$style_reset"
printf '%sInstaller · macOS %s · %s%s\n\n' \
  "$style_dim" "$macos_version" "$architecture_name" "$style_reset"

download() {
  local name="$1"
  local label="$2"
  status "Downloading $label..."
  curl --proto '=https' --tlsv1.2 \
    --fail --location --silent --show-error --retry 3 \
    --output "$work_dir/$name" "$download_base/$name"
}

download SHA256SUMS "release checksums"

verify_download() {
  local name="$1"
  local label="$2"
  local expected actual
  expected="$(awk -v name="$name" '$2 == name { print $1; exit }' "$work_dir/SHA256SUMS")"
  if [[ -z "$expected" ]]; then
    echo "No checksum was published for $name" >&2
    exit 1
  fi
  actual="$(shasum -a 256 "$work_dir/$name" | awk '{ print $1 }')"
  if [[ "$actual" != "$expected" ]]; then
    echo "Checksum verification failed for $name" >&2
    exit 1
  fi
  success "Verified $label"
}

verify_code_signature() {
  local path="$1"
  local label="$2"
  local log="$work_dir/codesign-$label.log"
  shift 2
  if ! codesign --verify "$@" "$path" >"$log" 2>&1; then
    echo "Code signature verification failed for $label" >&2
    cat "$log" >&2
    exit 1
  fi
  success "Verified $label code signature"
}

signature_field() {
  local path="$1"
  local field="$2"
  codesign -dv --verbose=4 "$path" 2>&1 | \
    awk -F= -v field="$field" '$1 == field { print $2; exit }'
}

app_is_running() {
  local pid executable
  while IFS= read -r pid; do
    [[ -n "$pid" ]] || continue
    executable="$(ps -p "$pid" -o comm= 2>/dev/null || true)"
    if [[ "$executable" == "$app_path/Contents/MacOS/UsageMenuBar" ]]; then
      return 0
    fi
  done < <(pgrep -x UsageMenuBar 2>/dev/null || true)
  return 1
}

daemon_pids() {
  local source pid executable command_line identifier
  {
    /usr/sbin/lsof -t -- "$daemon_socket.lock" 2>/dev/null | awk '{ print "lock " $1 }' || true
    pgrep -x usage-daemon 2>/dev/null | awk '{ print "scan " $1 }' || true
  } | sort -u -k2,2 | while read -r source pid; do
    [[ -n "$pid" ]] || continue
    executable="$(ps -p "$pid" -o comm= 2>/dev/null || true)"
    executable="${executable#${executable%%[![:space:]]*}}"
    [[ "${executable##*/}" == "usage-daemon" ]] || continue
    if [[ "$source" == "scan" ]]; then
      command_line="$(ps -ww -p "$pid" -o command= 2>/dev/null || true)"
      case "$command_line" in
        *" --socket-path $daemon_socket"|*" --socket-path $daemon_socket "*) ;;
        *) continue ;;
      esac
    fi
    if [[ "$source" != "lock" && "$executable" != "$app_path/Contents/MacOS/usage-daemon" ]]; then
      identifier="$(signature_field "$executable" Identifier 2>/dev/null || true)"
      [[ "$identifier" == "$expected_daemon_identifier" ]] || continue
    fi
    printf '%s\n' "$pid"
  done
}

stop_daemons() {
  local pid
  local pids=()
  while IFS= read -r pid; do
    [[ -n "$pid" ]] && pids+=("$pid")
  done < <(daemon_pids)
  [[ "${#pids[@]}" -gt 0 ]] || return 0

  echo "Stopping UsageTracker background service..."
  kill -TERM "${pids[@]}" 2>/dev/null || true
  if wait_for_processes 20 "${pids[@]}"; then return 0; fi

  for pid in "${pids[@]}"; do
    if kill -0 "$pid" 2>/dev/null; then kill -KILL "$pid" 2>/dev/null || true; fi
  done
  if wait_for_processes 20 "${pids[@]}"; then return 0; fi
  echo "UsageTracker background service could not be stopped." >&2
  return 1
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

expected_app_identifier="engineering.super.usagetracker"
expected_daemon_identifier="$expected_app_identifier.daemon"
expected_cli_identifier="$expected_app_identifier.cli"

if [[ "$install_app" == "1" ]]; then
  app_asset="UsageTracker-macos-$release_arch.zip"
  download "$app_asset" "app"
  verify_download "$app_asset" "app archive"
  if ! unzip -Z1 "$work_dir/$app_asset" | awk '
    BEGIN { valid = 1; found_app = 0 }
    /^UsageTracker\.app\// { found_app = 1; next }
    { valid = 0 }
    END { exit !(valid && found_app) }
  '; then
    echo "$app_asset contains files outside UsageTracker.app" >&2
    exit 1
  fi
  mkdir -p "$work_dir/app"
  ditto -x -k "$work_dir/$app_asset" "$work_dir/app"
  source_app="$work_dir/app/UsageTracker.app"
  if [[ ! -d "$source_app" ]]; then
    echo "$app_asset does not contain UsageTracker.app" >&2
    exit 1
  fi
  verify_code_signature "$source_app" "app" --deep --strict --verbose=2
  app_bundle_identifier="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' \
    "$source_app/Contents/Info.plist")"
  app_identifier="$(signature_field "$source_app" Identifier)"
  app_signature="$(signature_field "$source_app" Signature)"
  if [[ "$app_bundle_identifier" != "$expected_app_identifier" || \
        "$app_identifier" != "$expected_app_identifier" ]]; then
    echo "The app bundle or signing identifier is not UsageTracker" >&2
    exit 1
  fi
  if [[ "$app_signature" != "adhoc" ]]; then
    echo "The app does not have the expected ad-hoc signature" >&2
    exit 1
  fi
fi

if [[ "$install_cli" == "1" ]]; then
  cli_asset="usage-macos-$release_arch.tar.gz"
  download "$cli_asset" "command-line tool"
  verify_download "$cli_asset" "command-line archive"
  if [[ "$(tar -tzf "$work_dir/$cli_asset")" != "usage" ]]; then
    echo "$cli_asset does not contain exactly one usage executable" >&2
    exit 1
  fi
  mkdir -p "$work_dir/cli"
  tar -xzf "$work_dir/$cli_asset" -C "$work_dir/cli"
  source_cli="$work_dir/cli/usage"
  if [[ ! -f "$source_cli" || -L "$source_cli" || ! -x "$source_cli" ]]; then
    echo "$cli_asset does not contain an executable usage command" >&2
    exit 1
  fi
  verify_code_signature "$source_cli" "command-line tool" --strict --verbose=2
  cli_identifier="$(signature_field "$source_cli" Identifier)"
  cli_signature="$(signature_field "$source_cli" Signature)"
  if [[ "$cli_identifier" != "$expected_cli_identifier" ]]; then
    echo "The command signing identifier is not UsageTracker" >&2
    exit 1
  fi
  if [[ "$cli_signature" != "adhoc" ]]; then
    echo "The usage command does not have the expected ad-hoc signature" >&2
    exit 1
  fi
fi

if [[ "$install_app" == "1" ]]; then
  app_path="$app_dir/UsageTracker.app"
  if [[ -e "$app_path" ]]; then
    installed_bundle_id="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' \
      "$app_path/Contents/Info.plist" 2>/dev/null || true)"
    installed_app_identifier="$(signature_field "$app_path" Identifier || true)"
    if [[ "$force" != "1" && \
          ( "$installed_bundle_id" != "$app_identifier" || "$installed_app_identifier" != "$app_identifier" ) ]]; then
      echo "Refusing to replace an unrelated app at $app_path; pass --force to override" >&2
      exit 1
    fi
  fi
fi

if [[ "$install_cli" == "1" ]]; then
  cli_path="$bin_dir/usage"
  if [[ -e "$cli_path" ]]; then
    installed_cli_id="$(signature_field "$cli_path" Identifier || true)"
    if [[ "$force" != "1" && "$installed_cli_id" != "$cli_identifier" ]]; then
      echo "Refusing to replace an unrelated command at $cli_path; pass --force to override" >&2
      exit 1
    fi
  fi
fi

if [[ "$install_app" == "1" ]]; then
  status "Installing app..."
  if app_is_running; then
    app_was_running=1
    status "Closing the running app..."
    osascript -e "tell application id \"$app_identifier\" to quit" >/dev/null 2>&1 || true
    for _ in {1..20}; do
      if ! app_is_running; then break; fi
      sleep 0.25
    done
    if app_is_running; then
      echo "UsageTracker is still running. Quit it and run the installer again." >&2
      exit 1
    fi
  fi

  if ! stop_daemons; then
    exit 1
  fi

  mkdir -p "$app_dir"
  new_app="$app_dir/.UsageTracker.app.install.$$"
  backup_app="$app_dir/.UsageTracker.app.backup.$$"
  ditto "$source_app" "$new_app"
  if [[ -e "$app_path" ]]; then
    mv "$app_path" "$backup_app"
  else
    backup_app=""
  fi
  if ! mv "$new_app" "$app_path"; then
    if [[ -n "$backup_app" && -e "$backup_app" ]]; then
      mv "$backup_app" "$app_path"
      backup_app=""
    fi
    echo "Could not install UsageTracker.app" >&2
    exit 1
  fi
  new_app=""
  if [[ -n "$backup_app" ]]; then
    rm -rf "$backup_app"
    backup_app=""
  fi
  printf '%s\n%s\n' "$app_identifier" "$app_signature" \
    > "$app_dir/.UsageTracker.app.install-receipt"
  chmod 0600 "$app_dir/.UsageTracker.app.install-receipt"
  success "Installed app"
fi

if [[ "$install_cli" == "1" ]]; then
  status "Installing command-line tool..."
  mkdir -p "$bin_dir"
  cli_temp="$bin_dir/.usage.install.$$"
  install -m 0755 "$source_cli" "$cli_temp"
  mv -f "$cli_temp" "$cli_path"
  printf '%s\n%s\n' "$cli_identifier" "$cli_signature" \
    > "$bin_dir/.usage.install-receipt"
  chmod 0600 "$bin_dir/.usage.install-receipt"
  success "Installed command-line tool"
  case ":$PATH:" in
    *":$bin_dir:"*) ;;
    *) notice "Add $bin_dir to PATH to run 'usage' from your shell." ;;
  esac
fi

if [[ "$install_app" == "1" && "$launch_app" == "1" ]]; then
  open "$app_dir/UsageTracker.app" || true
fi

printf '\n'
success "Installation complete"
printf '\n'
if [[ "$install_app" == "1" ]]; then
  printf '  %-5s %s\n' "App" "$app_path"
fi
if [[ "$install_cli" == "1" ]]; then
  printf '  %-5s %s\n' "CLI" "$cli_path"
fi
printf '  %-5s %s (preserved)\n' "Data" "$daemon_home"
if [[ "$install_app" == "1" && "$launch_app" == "1" ]]; then
  printf '\n'
  notice "If macOS blocks the app, open System Settings → Privacy & Security and click Open Anyway."
fi
