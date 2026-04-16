#!/bin/bash
set -e

REPO_DIR="$(cd "$(dirname "$0")" && pwd)"
PLIST_NAME="com.user.tgbot-rust"
PLIST_SRC="$REPO_DIR/$PLIST_NAME.plist"
PLIST_DST="$HOME/Library/LaunchAgents/$PLIST_NAME.plist"

if [ ! -f "$PLIST_SRC" ]; then
    echo "Error: $PLIST_SRC not found"
    exit 1
fi

if [ ! -f "$REPO_DIR/target/release/tgbot" ]; then
    echo "Building release binary..."
    cargo build --release --manifest-path "$REPO_DIR/Cargo.toml"
fi

echo "Installing service plist..."
sed "s|/PATH/TO/tgbot/rust|$REPO_DIR|g" "$PLIST_SRC" > "$PLIST_DST"

echo "Loading service..."
launchctl load "$PLIST_DST"

echo "Done. Bot is running as a background service."
echo "  Logs: $REPO_DIR/tgbot.log"
echo "  Errors: $REPO_DIR/tgbot-error.log"
echo ""
echo "To stop:    launchctl unload $PLIST_DST"
echo "To restart: launchctl unload $PLIST_DST && launchctl load $PLIST_DST"
