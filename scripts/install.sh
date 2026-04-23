#!/bin/bash
set -euo pipefail

BOLD='\033[1m'
RED='\033[0;31m'
RESET='\033[0m'

DRIVER_NAME="Spaghetti.driver"
HAL_DIR="/Library/Audio/Plug-Ins/HAL"
BIN_DIR="/usr/local/bin"

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"

die() {
    echo -e "${RED}error: $*${RESET}" >&2
    exit 1
}

step() {
    echo -e "\n${BOLD}==> $*${RESET}"
}

# --- Prerequisites ---

[[ "$(uname)" == "Darwin" ]] || die "This script only runs on macOS."
command -v cargo &>/dev/null || die "'cargo' not found. Install Rust from https://rustup.rs"

echo -e "${BOLD}spaghetti install${RESET}"
echo "This will install the CoreAudio driver, CLI tools, and a LaunchAgent."
echo "sudo access is required to write to ${HAL_DIR} and ${BIN_DIR}."
echo ""
echo "Press Enter to continue (or Ctrl-C to abort)."
read -r

# Acquire sudo upfront so subsequent sudo calls don't prompt mid-script.
sudo -v

# Determine the target user (the person who ran sudo, not root).
TARGET_USER="${SUDO_USER:-$USER}"

cd "$PROJECT_DIR"

# --- Build ---

step "Building workspace (release)"
cargo build --release --workspace

# --- CoreAudio driver ---

step "Assembling driver bundle"
BUNDLE="target/release/${DRIVER_NAME}"
rm -rf "$BUNDLE"
mkdir -p "$BUNDLE/Contents/MacOS"
cp target/release/libspaghetti_asp.dylib "$BUNDLE/Contents/MacOS/"
cp crates/asp-driver/Info.plist "$BUNDLE/Contents/"

step "Signing driver (ad-hoc)"
# Ad-hoc signing is sufficient for local use. Replace '-' with a Developer ID
# certificate for distribution.
codesign --sign - --force "$BUNDLE"

step "Installing driver to ${HAL_DIR}"
sudo rm -rf "${HAL_DIR}/${DRIVER_NAME}"
sudo cp -R "$BUNDLE" "${HAL_DIR}/"

step "Restarting coreaudiod"
# launchctl kickstart is blocked by SIP on modern macOS, so kill the process
# and let launchd respawn it automatically.
sudo killall coreaudiod

# --- CLI binaries ---

step "Installing binaries to ${BIN_DIR}"
sudo cp target/release/spaghetti "${BIN_DIR}/spaghetti"
sudo cp target/release/spaghettid "${BIN_DIR}/spaghettid"

# --- LaunchAgent (delegated to the CLI so end users can manage it too) ---

step "Installing LaunchAgent for ${TARGET_USER}"
sudo -u "$TARGET_USER" "${BIN_DIR}/spaghetti" daemon install

# --- Done ---

echo ""
echo -e "${BOLD}Installation complete.${RESET}"
echo ""
echo "Getting started:"
echo "  Watch output:     spaghetti monitor"
echo "  List devices:     spaghetti devices"
echo "  CLI help:         spaghetti --help"
echo ""
echo "Manage the daemon:"
echo "  spaghetti daemon status"
echo "  spaghetti daemon restart"
echo "  spaghetti daemon logs -f"
echo "  spaghetti daemon stop"
echo "  spaghetti daemon start"
echo ""
echo "Verify the audio device appeared:"
echo "  system_profiler SPAudioDataType | grep -A2 Spaghetti"
