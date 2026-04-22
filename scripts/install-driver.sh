#!/bin/bash
set -euo pipefail

DRIVER_NAME="Spaghetti.driver"
INSTALL_DIR="/Library/Audio/Plug-Ins/HAL"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"

cd "$PROJECT_DIR"

echo "Building driver (release)..."
cargo build --release -p spaghetti-asp

echo "Assembling driver bundle..."
BUNDLE="target/release/${DRIVER_NAME}"
rm -rf "$BUNDLE"
mkdir -p "$BUNDLE/Contents/MacOS"
cp target/release/libspaghetti_asp.dylib "$BUNDLE/Contents/MacOS/"
cp crates/asp-driver/Info.plist "$BUNDLE/Contents/"

# Code signing (ad-hoc for local dev; replace with Developer ID for distribution)
echo "Signing driver (ad-hoc)..."
codesign --sign - --force "$BUNDLE"

echo "Installing to ${INSTALL_DIR}..."
sudo rm -rf "${INSTALL_DIR}/${DRIVER_NAME}"
sudo cp -R "$BUNDLE" "${INSTALL_DIR}/"

echo "Restarting coreaudiod..."
sudo launchctl kickstart -k system/com.apple.audio.coreaudiod

echo ""
echo "Done! Check if the device appeared:"
echo "  system_profiler SPAudioDataType | grep -A2 Spaghetti"
echo ""
echo "Monitor driver logs:"
echo "  log stream --predicate 'sender == \"coreaudiod\"' --level debug"
