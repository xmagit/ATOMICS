#!/bin/sh
# Build/install the desktop app, or remove just the installed files with --uninstall.
set -eu

case "${1:-}" in
  '') action=install ;;
  --uninstall) action=uninstall ;;
  --help|-h) echo 'Usage: scripts/linux-install.sh [--uninstall]'; exit 0 ;;
  *) echo 'Usage: scripts/linux-install.sh [--uninstall]' >&2; exit 2 ;;
esac
[ "$#" -le 1 ] || { echo 'Too many arguments' >&2; exit 2; }
[ "$(uname -s)" = Linux ] || { echo 'This installer requires Linux; use scripts/macos-app.sh on macOS.' >&2; exit 1; }

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
bin="$HOME/.local/bin"
case "${XDG_DATA_HOME:-}" in
  /*) data=$XDG_DATA_HOME ;;
  *) data="$HOME/.local/share" ;;
esac
desktop="$data/applications/atomics.desktop"
icon="$data/icons/hicolor/scalable/apps/atomics.svg"
icon_png="$data/icons/hicolor/512x512/apps/atomics.png"
# What builds from before the rename to ATOMICS installed.
remove_old_names() {
  rm -f -- "$bin/imacs" "$data/applications/imacs.desktop" "$data/icons/hicolor/scalable/apps/imacs.svg" \
    "$data/icons/hicolor/512x512/apps/imacs.png"
}

if [ "$action" = uninstall ]; then
  rm -f -- "$bin/atomics" "$desktop" "$icon" "$icon_png"
  remove_old_names
else
  cd "$root"
  target_dir=${CARGO_TARGET_DIR:-"$root/target"}
  host=$(rustc -vV | sed -n 's/^host: //p')
  case "$host" in
    *-linux-*) ;;
    *) echo "Expected a Linux Rust host, got: $host" >&2; exit 1 ;;
  esac
  cargo build --release --manifest-path "$root/Cargo.toml" -p atomics --target "$host" --target-dir "$target_dir"

  mkdir -p -- "$bin" "$(dirname -- "$desktop")" "$(dirname -- "$icon")" "$(dirname -- "$icon_png")"
  # Renaming a new inode avoids ETXTBSY when updating a running Linux binary.
  staged=$(mktemp "$bin/.atomics.XXXXXX")
  trap 'rm -f -- "$staged"' EXIT
  trap 'exit 1' HUP INT TERM
  install -m 755 -- "$target_dir/$host/release/atomics" "$staged"
  mv -f -- "$staged" "$bin/atomics"
  remove_old_names
  install -m 644 -- "$root/assets/icon.svg" "$icon"
  install -m 644 -- "$root/assets/icon.png" "$icon_png"
  # Desktop Entry Exec has its own quoting rules, including %% for a literal %.
  executable=$(printf '%s' "$bin/atomics" | sed 's/\\/\\\\/g; s/"/\\"/g; s/\$/\\$/g; s/`/\\`/g; s/%/%%/g; s/\\/\\\\/g')
  cat > "$desktop" <<EOF
[Desktop Entry]
Type=Application
Name=ATOMICS
Comment=Review-first workspace for coding agents
Exec="$executable"
Icon=atomics
Terminal=false
Categories=Development;IDE;
StartupNotify=true
StartupWMClass=atomics
EOF
  chmod 644 "$desktop"
fi

if command -v update-desktop-database >/dev/null 2>&1; then
  update-desktop-database "$data/applications" || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1 && [ -f "$data/icons/hicolor/index.theme" ]; then
  gtk-update-icon-cache -f -t "$data/icons/hicolor" || true
fi
if [ "$action" = uninstall ]; then
  echo 'Uninstalled ATOMICS. Configuration and session state were kept.'
else
  echo "Installed $bin/atomics and $desktop"
  case ":$PATH:" in
    *":$bin:"*) ;;
    *) echo "Add $bin to PATH to run atomics from a terminal." ;;
  esac
  # Use the installed path even before ~/.local/bin has been added to PATH.
  # Keep stdin attached: setup guides first-time installation and resumes on updates.
  "$bin/atomics" setup
fi
