#!/bin/sh
# Build the release app and install it into /Applications, replacing the
# previous local build. Meetings and settings live in
# ~/Library/Application Support/com.meetily.ai and are kept.
set -e
cd "$(dirname "$0")/.."

pnpm tauri build --bundles app -- --features coreml

APP="../target/release/bundle/macos/meetily.app"
[ -d "$APP" ] || { echo "Build output not found: $APP" >&2; exit 1; }

osascript -e 'tell application "meetily" to quit' 2>/dev/null || true
sleep 2
rm -rf /Applications/meetily.app
cp -R "$APP" /Applications/
echo "Installed $(defaults read /Applications/meetily.app/Contents/Info.plist CFBundleShortVersionString) to /Applications/meetily.app"
