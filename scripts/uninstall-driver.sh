#!/bin/bash
set -e

DRIVER_NAME="Spaghetti.driver"
INSTALL_DIR="/Library/Audio/Plug-Ins/HAL"
BIN_DIR="/usr/local/bin"

TARGET_USER="${SUDO_USER:-$USER}"

echo "Stopping and removing LaunchAgent..."
if [[ -x "${BIN_DIR}/spaghetti" ]]; then
    sudo -u "$TARGET_USER" "${BIN_DIR}/spaghetti" daemon uninstall || true
fi

echo "Removing ${DRIVER_NAME}..."
sudo rm -rf "${INSTALL_DIR}/${DRIVER_NAME}"

echo "Removing binaries..."
sudo rm -f "${BIN_DIR}/spaghettid" "${BIN_DIR}/spaghetti"

echo "Done. Restart coreaudiod to release the device:"
echo "  sudo killall coreaudiod"
