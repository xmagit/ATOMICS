#!/bin/sh
# Builds ATOMICS.app: the release binary, the atomisk.ai icon and an Info.plist.
# The app belongs to macOS once opened, so closing a terminal no longer quits it.
# Usage: scripts/macos-app.sh [destination folder, default ~/Applications]
set -eu
[ "$(uname -s)" = Darwin ] || { echo 'This packager requires macOS; use scripts/linux-install.sh on Linux.' >&2; exit 1; }

root=$(cd "$(dirname "$0")/.." && pwd)
dest=${1:-"$HOME/Applications"}
app="$dest/ATOMICS.app"

cargo build --release --manifest-path "$root/Cargo.toml" -p atomics
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -n 1)

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
iconset="$work/ATOMICS.iconset"
mkdir -p "$iconset"
render() { # size output
  if command -v rsvg-convert >/dev/null 2>&1; then
    rsvg-convert -w "$1" -h "$1" "$root/assets/icon.svg" -o "$2"
  else
    sips -z "$1" "$1" "$root/assets/icon.png" --out "$2" >/dev/null
  fi
}
for size in 16 32 128 256 512; do
  render "$size" "$iconset/icon_${size}x${size}.png"
  render $((size * 2)) "$iconset/icon_${size}x${size}@2x.png"
done

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$root/target/release/atomics" "$app/Contents/MacOS/atomics"
iconutil -c icns "$iconset" -o "$app/Contents/Resources/ATOMICS.icns"
cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>ATOMICS</string>
  <key>CFBundleDisplayName</key><string>ATOMICS</string>
  <key>CFBundleIdentifier</key><string>ai.atomisk.atomics</string>
  <key>CFBundleExecutable</key><string>atomics</string>
  <key>CFBundleIconFile</key><string>ATOMICS</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>LSMinimumSystemVersion</key><string>12.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSMicrophoneUsageDescription</key>
  <string>ATOMICS records your voice while you use the voice shortcut and sends the transcript to the chief agent.</string>
</dict>
</plist>
EOF

# Ad-hoc signature: lets macOS remember the microphone permission for the app.
codesign --force --deep --sign - "$app" >/dev/null 2>&1 || echo "codesign failed; the app still runs" >&2
# The bundle builds from before the rename to ATOMICS made.
rm -rf -- "$dest/IMACS.app"
echo "$app"
