#!/usr/bin/env bash
# Install the native 3D Modeler as an Omarchy app: binary, launcher entry,
# icon, .bee3d file association, and a Hyprland rule so the viewport stays
# fully opaque.
#
# Usage:
#   3dmodeler/scripts/install-omarchy.sh
#   3dmodeler/scripts/install-omarchy.sh --launch
#   3dmodeler/scripts/install-omarchy.sh --skip-build
#   3dmodeler/scripts/install-omarchy.sh --uninstall
#
# Environment:
#   PHYSICS3D_ROOT   If Physics3D is not a sibling of this repo, point here
#                    and the script will symlink it into place.

set -euo pipefail

APP_ID="3d-modeler"
APP_NAME="3D Modeler"
BIN_NAME="3d-modeler"
MCP_BIN_NAME="modeler-mcp"
PHYSICS3D_URL="https://github.com/bartbeecoders/physics3d.git"
HYPR_BEGIN="-- 3d-modeler-omarchy-install begin"
HYPR_END="-- 3d-modeler-omarchy-install end"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MODELER_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
REPO_ROOT="$(cd "$MODELER_ROOT/.." && pwd)"
PACKAGING="$MODELER_ROOT/packaging/omarchy"
PREFIX="${PREFIX:-$HOME/.local}"
BIN_DIR="$PREFIX/bin"
APP_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
ICON_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor/scalable/apps"
MIME_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/mime/packages"
HYPR_LUA="${XDG_CONFIG_HOME:-$HOME/.config}/hypr/hyprland.lua"
PHYSICS3D_EXPECTED="$(cd "$REPO_ROOT/.." && pwd)/Physics3D"

SKIP_BUILD=0
LAUNCH=0
UNINSTALL=0

usage() {
  cat <<EOF
Install $APP_NAME on Omarchy (launcher, icon, .bee3d files).

Usage: $(basename "$0") [options]

  --launch       Start the app after installing
  --skip-build   Reinstall launcher files using an existing release binary
  --uninstall    Remove the installed app, launcher, icon, and MIME type
  -h, --help     Show this help

Build dependencies are installed with \`omarchy pkg add\`. Rust is installed
with rustup if cargo is missing. Physics3D must sit next to this repo
(or set PHYSICS3D_ROOT).
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --launch) LAUNCH=1 ;;
    --skip-build) SKIP_BUILD=1 ;;
    --uninstall) UNINSTALL=1 ;;
    -h|--help) usage; exit 0 ;;
    *)
      echo "Unknown option: $1" >&2
      usage >&2
      exit 1
      ;;
  esac
  shift
done

info() { printf '\033[32m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[33m==>\033[0m %s\n' "$*"; }
die() { printf '\033[31merror:\033[0m %s\n' "$*" >&2; exit 1; }

require_omarchy() {
  command -v omarchy >/dev/null 2>&1 || die "Omarchy not found. This installer is for Omarchy Linux."
}

remove_hypr_block() {
  local file="$1"
  [[ -f "$file" ]] || return 0
  awk -v begin="$HYPR_BEGIN" -v end="$HYPR_END" '
    $0 == begin { skip=1; next }
    $0 == end { skip=0; next }
    !skip { print }
  ' "$file" >"$file.tmp"
  mv "$file.tmp" "$file"
}

uninstall() {
  require_omarchy
  info "Removing $APP_NAME"
  rm -f "$BIN_DIR/$BIN_NAME" "$BIN_DIR/$MCP_BIN_NAME"
  rm -f "$APP_DIR/${APP_ID}.desktop"
  rm -f "$ICON_DIR/${APP_ID}.svg"
  rm -f "$MIME_DIR/${APP_ID}.xml"
  if [[ -f "$HYPR_LUA" ]] && grep -qxF "$HYPR_BEGIN" "$HYPR_LUA"; then
    cp "$HYPR_LUA" "$HYPR_LUA.bak.$(date +%s)"
    remove_hypr_block "$HYPR_LUA"
    if command -v hyprctl >/dev/null 2>&1; then
      hyprctl reload >/dev/null 2>&1 || true
    fi
  fi
  update-desktop-database "$APP_DIR" >/dev/null 2>&1 || true
  update-mime-database "${XDG_DATA_HOME:-$HOME/.local/share}/mime" >/dev/null 2>&1 || true
  gtk-update-icon-cache "${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor" >/dev/null 2>&1 || true
  info "Uninstalled. Search the launcher (SUPER+SPACE) if a stale entry remains."
}

ensure_physics3d() {
  if [[ -d "$PHYSICS3D_EXPECTED/crates/aether-render" ]]; then
    return 0
  fi
  if [[ -n "${PHYSICS3D_ROOT:-}" ]]; then
    [[ -d "$PHYSICS3D_ROOT/crates/aether-render" ]] || \
      die "PHYSICS3D_ROOT=$PHYSICS3D_ROOT does not contain crates/aether-render"
    info "Linking Physics3D from $PHYSICS3D_ROOT"
    ln -sfn "$(cd "$PHYSICS3D_ROOT" && pwd)" "$PHYSICS3D_EXPECTED"
    return 0
  fi
  info "Cloning Physics3D next to this repo (required path dependency)"
  git clone --depth 1 "$PHYSICS3D_URL" "$PHYSICS3D_EXPECTED" || \
    die "Could not clone Physics3D. Clone it to $PHYSICS3D_EXPECTED or set PHYSICS3D_ROOT."
}

ensure_rust() {
  if [[ -f "$HOME/.cargo/env" ]]; then
    # shellcheck disable=SC1091
    source "$HOME/.cargo/env"
  fi
  if command -v cargo >/dev/null 2>&1 && command -v rustc >/dev/null 2>&1; then
    return 0
  fi
  info "Installing Rust via rustup"
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
  # shellcheck disable=SC1091
  source "$HOME/.cargo/env"
  command -v cargo >/dev/null 2>&1 || die "cargo still missing after rustup"
}

install_packages() {
  info "Ensuring build packages"
  omarchy pkg add cmake gcc make pkgconf vulkan-icd-loader clang llvm
}

build_app() {
  info "Building box3d (C library)"
  cmake -S "$REPO_ROOT" -B "$REPO_ROOT/build" \
    -DCMAKE_BUILD_TYPE=Release \
    -DBOX3D_SAMPLES=OFF \
    -DBOX3D_UNIT_TESTS=OFF \
    -DBOX3D_BENCHMARKS=OFF
  cmake --build "$REPO_ROOT/build" --config Release --parallel

  info "Building modeler-app + modeler-mcp (release)"
  (
    cd "$MODELER_ROOT"
    cargo build --release -p modeler-app -p modeler-mcp
  )
}

install_files() {
  local app_bin="$MODELER_ROOT/target/release/modeler-app"
  local mcp_bin="$MODELER_ROOT/target/release/modeler-mcp"
  [[ -x "$app_bin" ]] || die "Release binary not found at $app_bin (build first, or drop --skip-build)"

  mkdir -p "$BIN_DIR" "$APP_DIR" "$ICON_DIR" "$MIME_DIR"

  info "Installing binaries to $BIN_DIR"
  install -m 755 "$app_bin" "$BIN_DIR/$BIN_NAME"
  if [[ -x "$mcp_bin" ]]; then
    install -m 755 "$mcp_bin" "$BIN_DIR/$MCP_BIN_NAME"
  fi

  info "Installing icon and launcher"
  install -m 644 "$PACKAGING/${APP_ID}.svg" "$ICON_DIR/${APP_ID}.svg"
  install -m 644 "$PACKAGING/mime-bee3d.xml" "$MIME_DIR/${APP_ID}.xml"

  cat >"$APP_DIR/${APP_ID}.desktop" <<EOF
[Desktop Entry]
Version=1.0
Type=Application
Name=$APP_NAME
Comment=Blender-style 3D modeler with physics
Exec=$BIN_DIR/$BIN_NAME %F
Icon=$APP_ID
Terminal=false
Categories=Graphics;3DGraphics;Engineering;
Keywords=3d;modeler;blender;bee3d;cad;physics;
MimeType=application/x-bee3d;
StartupNotify=true
StartupWMClass=$APP_ID
EOF
  chmod +x "$APP_DIR/${APP_ID}.desktop"

  update-desktop-database "$APP_DIR" >/dev/null 2>&1 || true
  update-mime-database "${XDG_DATA_HOME:-$HOME/.local/share}/mime" >/dev/null 2>&1 || true
  gtk-update-icon-cache "${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor" >/dev/null 2>&1 || true
  xdg-mime default "${APP_ID}.desktop" application/x-bee3d >/dev/null 2>&1 || true
}

install_hypr_rule() {
  [[ -f "$HYPR_LUA" ]] || {
    warn "No $HYPR_LUA — skipping viewport opacity rule"
    return 0
  }
  if grep -qxF "$HYPR_BEGIN" "$HYPR_LUA"; then
    info "Hyprland window rule already present"
    return 0
  fi

  info "Adding Hyprland rule so the viewport stays opaque"
  cp "$HYPR_LUA" "$HYPR_LUA.bak.$(date +%s)"

  local block
  block=$(cat <<EOF

$HYPR_BEGIN
-- 3D Modeler: default window translucency washes out the viewport.
o.window("$APP_ID", { tag = "-default-opacity", opacity = "1 1" })
$HYPR_END
EOF
)

  local tmp inserted=0
  tmp="$(mktemp)"
  while IFS= read -r line || [[ -n "$line" ]]; do
    if ((inserted == 0)) && [[ "$line" == *hyprmoncfg-monitors.lua* ]]; then
      printf '%s\n\n' "$block"
      inserted=1
    fi
    printf '%s\n' "$line"
  done <"$HYPR_LUA" >"$tmp"
  if ((inserted == 0)); then
    printf '\n%s\n' "$block" >>"$tmp"
  fi
  mv "$tmp" "$HYPR_LUA"

  if command -v hyprctl >/dev/null 2>&1; then
    hyprctl reload >/dev/null
    local errors
    errors="$(hyprctl configerrors 2>/dev/null || true)"
    if [[ -n "$errors" && "$errors" != "no errors" ]]; then
      warn "hyprctl configerrors: $errors"
    fi
  fi
}

if ((UNINSTALL)); then
  uninstall
  exit 0
fi

[[ -d "$MODELER_ROOT/crates/modeler-app" ]] || die "Could not find modeler-app under $MODELER_ROOT"
[[ -f "$PACKAGING/${APP_ID}.svg" ]] || die "Missing packaging files in $PACKAGING"

require_omarchy
if ((SKIP_BUILD)); then
  info "Skipping build"
else
  ensure_physics3d
  install_packages
  ensure_rust
  build_app
fi

install_files
install_hypr_rule

cat <<EOF

$APP_NAME is installed.

  Launch:     SUPER+SPACE, type "$APP_NAME"
  Command:    $BIN_DIR/$BIN_NAME
  MCP server: $BIN_DIR/$MCP_BIN_NAME
              claude mcp add modeler -- $BIN_DIR/$MCP_BIN_NAME

EOF

if ((LAUNCH)); then
  info "Launching $APP_NAME"
  if command -v uwsm-app >/dev/null 2>&1; then
    setsid uwsm-app -- gtk-launch "${APP_ID}.desktop" >/dev/null 2>&1 &
  else
    setsid "$BIN_DIR/$BIN_NAME" >/dev/null 2>&1 &
  fi
fi
