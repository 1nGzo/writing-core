#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
PROJECT_DIR="$(cd -- "$SCRIPT_DIR/.." && pwd -P)"
SOURCE_BINARY="$PROJECT_DIR/src-tauri/target/release/caelorum-writing-spike"
INSTALL_DIR="$HOME/.local/lib/caelorum-writing-monitor"
INSTALL_BINARY="$INSTALL_DIR/caelorum-writing-spike"
BIN_DIR="$HOME/.local/bin"
LAUNCHER="$BIN_DIR/writing-monitor"
LAUNCHER_TEMPLATE="$PROJECT_DIR/scripts/writing-monitor.launcher"

if [[ ! -f "$SOURCE_BINARY" || ! -x "$SOURCE_BINARY" ]]; then
  printf '%s\n' 'Writing Monitor release executable not found.' >&2
  printf 'Expected: %s\n' "$SOURCE_BINARY" >&2
  printf 'Build the release version first from: %s\n' "$PROJECT_DIR" >&2
  printf '  npm run tauri build\n' >&2
  exit 1
fi

if [[ ! -f "$LAUNCHER_TEMPLATE" ]]; then
  printf 'Launcher template not found: %s\n' "$LAUNCHER_TEMPLATE" >&2
  exit 1
fi

existing_command="$(command -v writing-monitor 2>/dev/null || true)"
if [[ -n "$existing_command" && "$existing_command" = /* ]]; then
  existing_real="$(readlink -f "$existing_command" 2>/dev/null || printf '%s' "$existing_command")"
  launcher_real="$(readlink -f "$LAUNCHER" 2>/dev/null || printf '%s' "$LAUNCHER")"
  if [[ "$existing_real" != "$launcher_real" ]]; then
    printf 'Refusing to replace another writing-monitor command: %s\n' "$existing_command" >&2
    exit 1
  fi
fi

if [[ -L "$LAUNCHER" || ( -e "$LAUNCHER" && ! -f "$LAUNCHER" ) ]]; then
  printf 'Refusing to replace non-regular launcher path: %s\n' "$LAUNCHER" >&2
  exit 1
fi

if [[ -f "$LAUNCHER" ]] && ! grep -qF '# caelorum-writing-monitor launcher' "$LAUNCHER"; then
  printf 'Refusing to replace an unrelated launcher: %s\n' "$LAUNCHER" >&2
  exit 1
fi

mkdir -p "$INSTALL_DIR" "$BIN_DIR"
install -m 0755 "$SOURCE_BINARY" "$INSTALL_BINARY"
install -m 0755 "$LAUNCHER_TEMPLATE" "$LAUNCHER"

printf '%s\n' 'Installed Writing Monitor.'
printf 'Binary: %s\n' "$INSTALL_BINARY"
printf 'Command: %s\n' "$LAUNCHER"
if [[ ":${PATH}:" == *":$BIN_DIR:"* ]]; then
  printf 'PATH: %s is available in this shell.\n' "$BIN_DIR"
else
  printf 'PATH: %s is not in this shell yet; start a new login shell or source ~/.profile.\n' "$BIN_DIR"
fi
