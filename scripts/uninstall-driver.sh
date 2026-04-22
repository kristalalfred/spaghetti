#!/bin/bash
set -e
DRIVER_NAME="Spaghetti.driver"
INSTALL_DIR="/Library/Audio/Plug-Ins/HAL"

echo "Removing ${DRIVER_NAME}..."
sudo rm -rf "${INSTALL_DIR}/${DRIVER_NAME}"
echo "Removed. Restart coreaudiod to take effect."
