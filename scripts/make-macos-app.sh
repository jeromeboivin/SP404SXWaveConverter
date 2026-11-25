#!/usr/bin/env bash
# Usage: make-macos-app.sh <binary> <version> <output-dir>
set -euo pipefail
bin=$1; version=$2; out=$3
app="$out/SP404SX Wave Converter.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$bin" "$app/Contents/MacOS/sp404sx-wave-converter"

iconset=$(mktemp -d)/icon.iconset
mkdir -p "$iconset"
for s in 16 32 128 256; do
  sips -z $s $s assets/icon-256.png --out "$iconset/icon_${s}x${s}.png" >/dev/null
  sips -z $((s*2)) $((s*2)) icon.png --out "$iconset/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns "$iconset" -o "$app/Contents/Resources/icon.icns"

cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleName</key><string>SP404SX Wave Converter</string>
<key>CFBundleDisplayName</key><string>SP404SX Wave Converter</string>
<key>CFBundleIdentifier</key><string>io.github.jeromeboivin.sp404sx-wave-converter</string>
<key>CFBundleExecutable</key><string>sp404sx-wave-converter</string>
<key>CFBundleIconFile</key><string>icon</string>
<key>CFBundleVersion</key><string>$version</string>
<key>CFBundleShortVersionString</key><string>$version</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>NSHighResolutionCapable</key><true/>
<key>LSMinimumSystemVersion</key><string>11.0</string>
</dict></plist>
PLIST
# Ad-hoc sign so Apple Silicon will run it (users still need right-click > Open).
codesign --force --deep --sign - "$app"
