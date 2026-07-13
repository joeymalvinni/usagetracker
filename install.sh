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

usage() {
  cat <<'EOF'
Install UsageTracker from a signed GitHub release.

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
    ;;
  x86_64)
    release_arch="x86_64"
    ;;
  *)
    echo "Unsupported Mac architecture: $machine" >&2
    exit 1
    ;;
esac

for command in curl ditto shasum tar; do
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
  rm -rf "$work_dir"
  if [[ -n "$new_app" ]]; then rm -rf "$new_app"; fi
  if [[ -n "$backup_app" && -e "$backup_app" ]]; then
    echo "A previous app was preserved at $backup_app" >&2
  fi
}
trap cleanup EXIT

download() {
  local name="$1"
  echo "Downloading $name..."
  curl --proto '=https' --tlsv1.2 \
    --fail --location --silent --show-error --retry 3 \
    --output "$work_dir/$name" "$download_base/$name"
}

download SHA256SUMS

verify_download() {
  local name="$1"
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
  echo "Verified $name"
}

signature_field() {
  local path="$1"
  local field="$2"
  codesign -dv --verbose=4 "$path" 2>&1 | \
    awk -F= -v field="$field" '$1 == field { print $2; exit }'
}

if [[ "$install_app" == "1" ]]; then
  app_asset="UsageTracker-macos-$release_arch.zip"
  download "$app_asset"
  verify_download "$app_asset"
  mkdir -p "$work_dir/app"
  ditto -x -k "$work_dir/$app_asset" "$work_dir/app"
  source_app="$work_dir/app/UsageTracker.app"
  if [[ ! -d "$source_app" ]]; then
    echo "$app_asset does not contain UsageTracker.app" >&2
    exit 1
  fi
  codesign --verify --deep --strict --verbose=2 "$source_app"
  app_identifier="$(signature_field "$source_app" Identifier)"
  if [[ -z "$app_identifier" ]]; then
    echo "The app does not have a signing identifier" >&2
    exit 1
  fi
  if ! spctl --assess --type execute --verbose=2 "$source_app"; then
    echo "Gatekeeper rejected the downloaded app" >&2
    exit 1
  fi
  app_team="$(signature_field "$source_app" TeamIdentifier)"
  if [[ -z "$app_team" || "$app_team" == "not set" ]]; then
    echo "The app does not have a Developer ID team identifier" >&2
    exit 1
  fi
fi

if [[ "$install_cli" == "1" ]]; then
  cli_asset="usage-macos-$release_arch.tar.gz"
  download "$cli_asset"
  verify_download "$cli_asset"
  mkdir -p "$work_dir/cli"
  tar -xzf "$work_dir/$cli_asset" -C "$work_dir/cli"
  source_cli="$work_dir/cli/usage"
  if [[ ! -x "$source_cli" ]]; then
    echo "$cli_asset does not contain an executable usage command" >&2
    exit 1
  fi
  codesign --verify --strict --verbose=2 "$source_cli"
  cli_identifier="$(signature_field "$source_cli" Identifier)"
  if [[ -z "$cli_identifier" ]]; then
    echo "The usage command does not have a signing identifier" >&2
    exit 1
  fi
  cli_team="$(signature_field "$source_cli" TeamIdentifier)"
  if [[ -z "$cli_team" || "$cli_team" == "not set" ]]; then
    echo "The usage command does not have a Developer ID team identifier" >&2
    exit 1
  fi
  if [[ "$install_app" == "1" && "$cli_identifier" != "$app_identifier.cli" ]]; then
    echo "The app and CLI signing identifiers do not belong to the same release" >&2
    exit 1
  fi
  if [[ "$install_app" == "1" && "$cli_team" != "$app_team" ]]; then
    echo "The app and CLI were signed by different Apple Developer teams" >&2
    exit 1
  fi
fi

if [[ "$install_app" == "1" ]]; then
  app_path="$app_dir/UsageTracker.app"
  if [[ -e "$app_path" ]]; then
    installed_bundle_id="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' \
      "$app_path/Contents/Info.plist" 2>/dev/null || true)"
    installed_app_team="$(signature_field "$app_path" TeamIdentifier || true)"
    if [[ "$force" != "1" && \
          ( "$installed_bundle_id" != "$app_identifier" || "$installed_app_team" != "$app_team" ) ]]; then
      echo "Refusing to replace an unrelated app at $app_path; pass --force to override" >&2
      exit 1
    fi
  fi
fi

if [[ "$install_cli" == "1" ]]; then
  cli_path="$bin_dir/usage"
  if [[ -e "$cli_path" ]]; then
    installed_cli_id="$(signature_field "$cli_path" Identifier || true)"
    installed_cli_team="$(signature_field "$cli_path" TeamIdentifier || true)"
    if [[ "$force" != "1" && \
          ( "$installed_cli_id" != "$cli_identifier" || "$installed_cli_team" != "$cli_team" ) ]]; then
      echo "Refusing to replace an unrelated command at $cli_path; pass --force to override" >&2
      exit 1
    fi
  fi
fi

if [[ "$install_app" == "1" ]]; then
  if pgrep -x UsageMenuBar >/dev/null 2>&1; then
    echo "Asking UsageTracker to quit before updating..."
    osascript -e "tell application id \"$app_identifier\" to quit" >/dev/null 2>&1 || true
    for _ in {1..20}; do
      if ! pgrep -x UsageMenuBar >/dev/null 2>&1; then break; fi
      sleep 0.25
    done
    if pgrep -x UsageMenuBar >/dev/null 2>&1; then
      echo "UsageTracker is still running. Quit it and run the installer again." >&2
      exit 1
    fi
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
  printf '%s\n%s\n' "$app_identifier" "$app_team" \
    > "$app_dir/.UsageTracker.app.install-receipt"
  chmod 0600 "$app_dir/.UsageTracker.app.install-receipt"
  echo "Installed $app_path"
fi

if [[ "$install_cli" == "1" ]]; then
  mkdir -p "$bin_dir"
  cli_temp="$bin_dir/.usage.install.$$"
  install -m 0755 "$source_cli" "$cli_temp"
  mv -f "$cli_temp" "$cli_path"
  printf '%s\n%s\n' "$cli_identifier" "$cli_team" \
    > "$bin_dir/.usage.install-receipt"
  chmod 0600 "$bin_dir/.usage.install-receipt"
  echo "Installed $cli_path"
  case ":$PATH:" in
    *":$bin_dir:"*) ;;
    *) echo "Add $bin_dir to PATH to run 'usage' from your shell." ;;
  esac
fi

if [[ "$install_app" == "1" && "$launch_app" == "1" ]]; then
  open "$app_dir/UsageTracker.app"
fi

echo "UsageTracker installation complete. Existing ~/.usagetracker data was left untouched."
