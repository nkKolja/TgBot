#!/bin/bash
set -e

REPO_DIR="$(cd "$(dirname "$0")" && pwd)"
PLIST_NAME="com.user.tgbot-rust"
PLIST_SRC="$REPO_DIR/$PLIST_NAME.plist"
PLIST_DST="$HOME/Library/LaunchAgents/$PLIST_NAME.plist"
BINARY="$REPO_DIR/target/release/tgbot"

usage() {
    echo "Usage: $0 {install|update|uninstall|status}"
    echo ""
    echo "  install    Build and register the service (first-time setup)"
    echo "  update     Rebuild and restart the running service"
    echo "  uninstall  Stop the service and remove the plist"
    echo "  status     Show whether the service is loaded/running"
    exit 1
}

build() {
    echo "Building release binary..."
    cargo build --release --manifest-path "$REPO_DIR/Cargo.toml"
}

service_is_loaded() {
    launchctl list "$PLIST_NAME" &>/dev/null
}

stop_service() {
    if service_is_loaded; then
        echo "Stopping service..."
        launchctl unload "$PLIST_DST" 2>/dev/null || true
    fi
}

start_service() {
    echo "Starting service..."
    launchctl load "$PLIST_DST"
}

do_install() {
    if [ ! -f "$PLIST_SRC" ]; then
        echo "Error: $PLIST_SRC not found"
        exit 1
    fi

    build

    echo "Installing service plist..."
    sed "s|/PATH/TO/tgbot/rust|$REPO_DIR|g" "$PLIST_SRC" > "$PLIST_DST"

    start_service

    echo ""
    echo "Done. Bot is running as a background service."
    echo "  Logs:   $REPO_DIR/tgbot.log"
    echo "  Errors: $REPO_DIR/tgbot-error.log"
    echo ""
    echo "  Update:    $0 update"
    echo "  Uninstall: $0 uninstall"
}

do_update() {
    if [ ! -f "$PLIST_DST" ]; then
        echo "Service not installed. Run '$0 install' first."
        exit 1
    fi

    stop_service
    build
    start_service

    echo ""
    echo "Updated and restarted."
}

do_uninstall() {
    stop_service

    if [ -f "$PLIST_DST" ]; then
        rm "$PLIST_DST"
        echo "Removed $PLIST_DST"
    fi

    echo "Service uninstalled."
}

do_status() {
    if service_is_loaded; then
        echo "Service is loaded."
        launchctl list "$PLIST_NAME"
    else
        echo "Service is not loaded."
    fi
}

case "${1:-}" in
    install)   do_install ;;
    update)    do_update ;;
    uninstall) do_uninstall ;;
    status)    do_status ;;
    *)         usage ;;
esac
