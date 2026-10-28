#!/usr/bin/env bash
# Fetch the UP-AirQuality vendor firmware images for analysis / reflash.
#
# These images are Ubiquiti proprietary firmware and are intentionally NOT
# committed to this repo. They are not required to run the app (the collector
# speaks the sensor's network protocol; it never touches the firmware image) —
# they're only useful if you want to repeat the reverse engineering (`strings`)
# or attempt a reflash (see REFLASH.md).
#
# They are publicly downloadable from Ubiquiti's firmware CDN. The current list
# is in the feed:
#   https://fw-update.ui.com/api/firmware?filter=eq~~product~~UP-AirQuality&sort=version&limit=999
set -euo pipefail
cd "$(dirname "$0")"

declare -A FW=(
  [upaq-1.0.12.bin]="https://fw-download.ubnt.com/data/UP-AirQuality/3706-ESP32-1.0.12-f0fc1197-0dda-430e-b184-ae0d80dc2c00.bin"
  [upaq-1.0.16.bin]="https://fw-download.ubnt.com/data/UP-AirQuality/4a91-ESP32-1.0.16-1510e904-2307-43b0-9699-a4aa02a9ea49.bin"
)

for out in "${!FW[@]}"; do
  if [[ -f "$out" ]]; then echo "have $out"; continue; fi
  echo "fetching $out ..."
  curl -fL --retry 2 -o "$out" "${FW[$out]}"
done
echo "done. analyse with:  strings -n 6 upaq-1.0.12.bin | less"
