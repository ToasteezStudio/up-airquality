# Custom open-source firmware — backup + feasibility + procedure

**Read this fully before touching the hardware.** The order of operations
matters: back up first, decide second, flash last.

## Verdict up front
- The sensor is a **classic ESP32** (LX6) running an ESP-IDF app, sensor chip
  **Sensirion SEN66**, with a standard **dual-OTA** partition layout.
- Writing open-source firmware for this chip is easy. **Getting it onto the
  device is gated entirely by whether Secure Boot / Flash Encryption eFuses are
  burned — which cannot be read from the firmware image, only from the chip over
  UART.** Ubiquiti commonly burns them. If they are burned, unsigned custom
  firmware is rejected by the bootloader (UART *and* OTA) and a flash dump is
  AES-encrypted. Then custom firmware is **not viable** without Ubiquiti's keys.
- OTA is `esp_https_ota` (HTTPS pull). If Secure Boot is **off**, a custom image
  could in principle be pushed via the `updateFirmware` console action with **no
  soldering** — but confirming Secure Boot is off still needs the UART check.

## Caveats before you start
- Reflashing **voids the return/exchange** filed 2026-10-27 (order ~#UK6455047)
  and the warranty. The non-invasive fake-console route (this repo's Rust tool)
  reads the sensor with firmware untouched — prefer it unless you specifically
  want full local control.
- **3.3 V only.** A 5 V USB-UART adapter will destroy the ESP32. Do not power the
  board from the UART 3V3 pin and PoE at the same time.

## Hardware you need
- 3.3 V USB-to-UART adapter (CP2102 / FTDI set to 3.3 V).
- Jumper wires / pogo pins; fine-tip soldering iron if no test pads.
- `esptool` + `espefuse`: `pip install esptool` (or `brew install esptool`).

## Wiring (classic ESP32)
Find these pads on the ESP32 module and connect to the adapter:
| ESP32 | Adapter |
|---|---|
| GND | GND |
| 3V3 | 3V3 (only if not PoE-powered) |
| TX0 (GPIO1) | RX |
| RX0 (GPIO3) | TX |
| GPIO0 | GND **at power-on** to enter download mode |
| EN | pulse to GND to reset |

Enter download mode: hold GPIO0 → GND, pulse EN → GND, release EN, release GPIO0.

## Step 1 — identify the chip (read-only)
```
esptool --port /dev/tty.usbserial-XXXX flash_id
esptool --port /dev/tty.usbserial-XXXX chip_id
```
Note the **flash size** (e.g. 4MB = `0x400000`) — needed for the dump.

## Step 2 — read the security state (READ-ONLY, decides everything)
```
espefuse --port /dev/tty.usbserial-XXXX summary
```
Look at:
- `FLASH_CRYPT_CNT` — nonzero ⇒ **flash encryption ON** (dump is ciphertext).
- `ABS_DONE_0` — set ⇒ **Secure Boot V1 ON**.
- `ABS_DONE_1` / secure-boot-v2 fields — set ⇒ Secure Boot V2 ON.

**Decision:**
- Any of the above set → **custom firmware is blocked.** Stop; keep stock
  firmware + the fake-console reader. Still do Step 3 so you have a restorable
  backup.
- All clear → custom firmware is viable; continue.

## Step 3 — FULL BACKUP (do this regardless of the decision)
Replace `0x400000` with the real flash size from Step 1.
```
mkdir -p backup
esptool --port /dev/tty.usbserial-XXXX --baud 460800 \
  read_flash 0x0 0x400000 backup/upaq-flash-backup-$(date +%Y%m%d).bin
shasum -a 256 backup/upaq-flash-backup-*.bin > backup/upaq-flash-backup.sha256
```
Keep this file safe — it is the only way back to stock. (The `firmware/*.bin`
images in this repo are the vendor's **downloadable OTA app images**, not a dump
of *your* chip, and are not a substitute for this backup.)

Optionally also dump the eFuses and NVS separately for reference.

## Step 4 — flash custom firmware (only if Step 2 said "all clear")
Build from `firmware/` (see that folder's README), then:
```
esptool --port /dev/tty.usbserial-XXXX --baud 460800 erase_flash
esptool --port /dev/tty.usbserial-XXXX --baud 460800 write_flash 0x0 <your-build.bin>
```
(ESPHome/`idf.py flash` will pick sensible offsets; `0x0` is for a merged image.)

## Restore to stock
```
esptool --port /dev/tty.usbserial-XXXX --baud 460800 \
  write_flash 0x0 backup/upaq-flash-backup-YYYYMMDD.bin
```
This only restores cleanly if the dump was taken from this same chip. If flash
encryption was on, restoring the encrypted dump to the same chip works; moving it
to another chip does not.

## Why not just OTA a custom image?
If Secure Boot is off, `updateFirmware` (console → device, HTTPS pull from a URL
we host) could flash custom firmware without opening the case. But you still must
open the case once to run Step 2 and prove Secure Boot is off, and you want the
Step 3 backup before any write. So UART first regardless.
