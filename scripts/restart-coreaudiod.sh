#!/bin/bash
set -e
echo "Restarting coreaudiod..."
sudo killall coreaudiod
echo "Done. coreaudiod restarted."
