# firmware/

> **The vendor `.bin` images are NOT committed to this repo** — they're Ubiquiti
> proprietary firmware and are not needed to run the app (the collector speaks
> the sensor's network protocol; it never reads the firmware image). If you want
> them for analysis or a reflash, run `./fetch-firmware.sh` to pull them from
> Ubiquiti's public CDN.

## What the `.bin` files are (and are NOT)
`upaq-1.0.12.bin` and `upaq-1.0.16.bin` are the **vendor's downloadable OTA app
images**, pulled from `fw-download.ubnt.com` (feed:
`https://fw-update.ui.com/api/firmware?filter=eq~~product~~UP-AirQuality`).
`1.0.12` is the version running on the unit here.

They are plain, **unencrypted ESP-IDF app images** (magic `0xE9`) — useful for
`strings`/analysis. They are **NOT** a dump of your device's flash and are **not
a backup**. A real backup comes from `esptool read_flash` over UART — see
`../docs/REFLASH.md` Step 3, which writes to `../backup/`.

Analyse without executing:
```
strings -n 6 upaq-1.0.12.bin | less
```

## Two routes to custom firmware
1. **ESPHome** (`up-airquality.yaml`) — fastest if SEN6x support exists in your
   ESPHome version and you can map the GPIOs. See the TODOs in the YAML.
2. **ESP-IDF + Sensirion driver** — use when ESPHome lacks SEN66. Start a normal
   ESP-IDF project and add Sensirion's `embedded-i2c-sen66`
   (https://github.com/Sensirion/embedded-i2c-sen66). Wire I2C, read the SEN66,
   publish over MQTT/HTTP/Prometheus. This also gives you Ethernet (esp_eth)
   control for the PoE link.

Both are gated on `../docs/REFLASH.md` Step 2 (Secure Boot OFF) and Step 3
(verified backup) first.

## Firmware feed for siblings
The same feed lists `up-chime`, `UP-Siren-PoE`, `up-horn-speaker`, and the `UA-*`
ESP32 images — reverse-engineerable the same way.
