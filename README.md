# up-airquality

A small self-hosted collector + dashboard for the **Ubiquiti UniFi Protect
UP-AirQuality** sensor — **no UniFi Protect console, NVR or gateway required**.

It speaks the sensor's own protocol: it acts as a minimal "console" the sensor
adopts and dials back into, then shows the live readings (CO₂, PM1/2.5/4/10,
VOC, NOx, tVOC, temperature, humidity, AQI, vape) on a web dashboard and,
optionally, stores the history in TimescaleDB with Grafana/Prometheus.

> Runs against **stock sensor firmware** — nothing is flashed or modified on the
> device. Not affiliated with or endorsed by Ubiquiti. Use on hardware you own.

---

## Install with Homebrew (macOS)

```bash
brew install toasteezstudio/tap/up-airquality
```

This builds from source (about a minute). Then follow the Quickstart below,
using `up-airquality` in place of `./target/release/up-airquality`.

## Requirements

- **Rust** via [rustup](https://rustup.rs) (only when building from source). The toolchain is pinned in
  `rust-toolchain.toml` (currently 1.98.1); rustup installs it automatically
  on the first build.
- A **UP-AirQuality** sensor powered on your network (any 802.3af PoE switch or
  injector — it does **not** need UniFi networking gear), reachable by IP.
- The machine running this must be reachable **from the sensor on tcp/7442**
  (same subnet = automatic; across VLANs/firewalls you need an allow rule).

## Build

```bash
git clone https://github.com/ToasteezStudio/up-airquality.git
cd up-airquality
cargo build --release
# binary: target/release/up-airquality
```

On a Linux build host you can produce a fully static binary instead (handy for
copying to a small server or container with an older glibc):

```bash
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
# binary: target/x86_64-unknown-linux-musl/release/up-airquality
```

### macOS

Builds natively on Apple Silicon and Intel Macs. You need the Xcode Command
Line Tools (for the linker) and rustup, and nothing from Homebrew:

```bash
xcode-select --install     # skip if already installed
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
git clone https://github.com/ToasteezStudio/up-airquality.git
cd up-airquality
cargo build --release
# binary: target/release/up-airquality
```

The first time you run `run` or `serve`, macOS may ask whether to allow
incoming network connections. Click **Allow**: the sensor has to dial in on
tcp/7442. You can change it later under System Settings → Network → Firewall.
The collector stops while the Mac sleeps, so for round-the-clock history run it
on a machine that stays awake.

## Quickstart (3 steps)

Replace `<SENSOR_IP>` with your sensor's IP and `<THIS_IP>` with the LAN IP of
the machine you're running this on.

```bash
# 1. Confirm you can see the sensor (read-only):
./target/release/up-airquality --host <SENSOR_IP> info

# 2. Adopt it to this machine and start the dashboard + console:
./target/release/up-airquality --host <SENSOR_IP> run --console-ip <THIS_IP>

# 3. Open the dashboard (xdg-open on Linux):
open http://localhost:8099/      # or http://<THIS_IP>:8099/
```

Within a few seconds the sensor dials back and the cards fill with live data.
`run` auto-detects `--console-ip` if you omit it; pass it explicitly if the
machine has several interfaces.

Adoption is remembered by the sensor, so next time start the collector with
`serve` instead of `run`:

```bash
./target/release/up-airquality --host <SENSOR_IP> serve
```

**Finding the sensor's IP:** check your DHCP/router client list for a host like
`UP-AirQuality-xxxxxx`, or scan for an HTTPS service on tcp/8080.

### Several sensors

One collector handles several sensors, for example one per floor or room.
Pass `--host` once per sensor, or as a comma-separated list:

```bash
# Adopt all three (once), then serve them:
./target/release/up-airquality --host 10.0.30.11,10.0.30.12,10.0.30.13 run
./target/release/up-airquality --host 10.0.30.11,10.0.30.12,10.0.30.13 serve
```

Each sensor is identified by its MAC address. The dashboard shows a section
per sensor, `/readings.json` lists them under `sensors`, and every
`/metrics` series carries a `sensor="<MAC>"` label. The database already
records which sensor each row came from in its `source` column (the sensor's
IP address).

### A sensor that's already adopted somewhere else

A sensor reports to one console at a time. If it's already adopted by UniFi
Protect or another collector, release it first, then adopt it here:

1. **Release it from the system that has it.**
   - **UniFi Protect:** in the sensor's settings, under *Manage*, choose
     **Unmanage**. This removes it from Protect *and* restores it to factory
     defaults, so you don't need to touch the sensor.
   - **No access to that system** (for example a second-hand sensor):
     factory-reset it with its **Reset button**. Keep it powered over PoE and
     hold the button for 5–10 seconds, until the LED shows the restore has
     started. [techspecs.ui.com](https://techspecs.ui.com) shows where the
     button is. A physical reset doesn't remove the sensor from the old
     system's device list, so delete it there too if you can.
2. **Adopt it here** with `up-airquality --host <SENSOR_IP> run`, as in the
   Quickstart. A reset sensor may get a new IP address from DHCP.

The sensor's own API has no "unadopt" call, so up-airquality can't release a
sensor that another system owns. That's why it's done from that system or with
the Reset button. See Ubiquiti's
[factory reset guide](https://help.ui.com/hc/en-us/articles/205143490).

## Commands

| Command | What it does |
|---|---|
| `info` | Read-only identity probe (`POST /api/info`). Safe. |
| `locate` | Blink the status LED (only works once adopted). |
| `serve` | Run the console + dashboard for an **already-adopted** sensor. Pass `--host <SENSOR_IP>` so only the sensor can connect. |
| `run` | Adopt the sensor to this machine, then serve. **Changes device state** (recover with a factory reset). |

Flags and environment. Put `--host`, `--user` and `--pass` **before** the
command (`up-airquality --host <SENSOR_IP> serve`); `--http-port` and `--pg`
can go anywhere.

- `--host <ip>`: sensor IP; repeat it or comma-separate for several sensors.
  Required for `info`/`locate`/`run`, which act on each sensor. With `serve`,
  the console only accepts connections from these addresses (recommended).
- `--user` / `--pass` — login (default `ubnt` / `ubnt`, the vendor pre-adoption creds).
- `--http-port <n>` — dashboard/metrics port (default `8099`).
- `--pg "<dsn>"` or `UPAQ_PG_DSN` — optional Postgres/TimescaleDB DSN to persist readings
  (e.g. `host=db.example port=5432 user=upaq password=… dbname=airquality`). Omit to run DB-less.
  Prefer `UPAQ_PG_DSN`, because command-line arguments are visible to other local users.
  Add `sslmode=require` to the DSN for TLS. The server certificate is verified against
  the system trust store, or against a PEM CA bundle given in `UPAQ_PG_CA`.
- `--mqtt "<url>"` or `UPAQ_MQTT_URL` — optional MQTT broker for Home Assistant
  (see below): `mqtt://[user:pass@]host[:1883]`. MQTT over TLS (`mqtts://`)
  is not supported yet. Prefer the environment variable, for the same reason
  as `UPAQ_PG_DSN`.
- `--mqtt-discovery-prefix <prefix>` — Home Assistant discovery prefix
  (default `homeassistant`).
- `RUST_LOG=debug` — verbose protocol logging.

## Home Assistant

Point the collector at the MQTT broker Home Assistant uses (for example the
Mosquitto add-on) and each sensor appears in Home Assistant on its own as a
device, with every reading as a correctly typed entity. No YAML is needed.

```bash
UPAQ_MQTT_URL='mqtt://<user>:<password>@<broker>:1883' \
  ./target/release/up-airquality --host <SENSOR_IP> serve
```

- Uses [MQTT Discovery](https://www.home-assistant.io/integrations/mqtt/#mqtt-discovery):
  one device per sensor, identified by its MAC address, so any number of
  sensors work.
- Entities: CO₂, PM1/PM2.5/PM4/PM10, temperature, humidity, AQI and total VOC
  with their Home Assistant device classes and units, plus the VOC, NOx and
  vape indices (unitless).
- If a sensor stops reporting for 5 minutes its entities show as unavailable.
  If the collector itself goes offline they all do, via an MQTT last will on
  `up-airquality/status`.
- Readings are also published as JSON on `up-airquality/<mac>/state`, for
  anything else that speaks MQTT.
- Special characters in the user name or password must be percent-encoded in
  the URL (`@` → `%40`, `:` → `%3A`).

## Dashboard & metrics

`serve`/`run` expose:
- `/` — live dashboard (auto-updating cards with healthy-range guidance per metric).
- `/readings.json` — latest readings as JSON: a `sensors` array with one entry
  per sensor (`id`, `source`, `reading`, `updated_ms`, `age_secs`, `count`).
  The top-level fields repeat the most recently updated sensor, for clients
  that expect a single sensor.
- `/metrics` — Prometheus gauges (`upaq_co2_ppm`, `upaq_pm2p5_ugm3`, …), one
  series per sensor, labelled `sensor="<MAC>"`.
- `/healthz` — returns `ok` (for uptime checks).

Readings refresh every few seconds. Native apps and scripts (e.g. a WidgetKit
widget) can poll `/readings.json`; web pages served from other origins can't,
because the dashboard sends no CORS header.

## Security

Designed for a trusted home or office LAN:

- **Sensor console (tcp/7442).** Run `serve` with `--host <SENSOR_IP>` (`run`
  does this automatically) so connections from any other address are dropped
  before TLS and nothing else can inject readings. Message sizes,
  decompression, connection counts and handshake/idle time are all capped.
- **Dashboard (tcp/8099)** is read-only and has **no authentication**: anyone
  who can reach the port can see the readings. Firewall it to the machines that
  need it, such as your Prometheus server. It sends no CORS header and loads
  nothing from third parties.
- **Sensor API.** Adopting talks to the sensor over HTTPS with the vendor's
  default credentials and accepts its self-signed certificate, because the
  device generates one per unit and there is nothing to verify it against.
  Do this on a network you trust.
- **Database.** Use `sslmode=require` if readings cross an untrusted network.
- **MQTT.** Use a broker account with a password, and keep the broker on a
  network you trust: MQTT over TLS isn't supported yet, so readings and the
  broker password travel unencrypted. Credentials are never logged.

## Optional: history (TimescaleDB) + Grafana + Prometheus

For long-term history and graphs:
1. Create the database/schema: `db/schema.sql` (TimescaleDB hypertable + 5-min
   rollup + 90-day retention, with example least-privilege role grants in the
   comments at the end).
2. Run the collector with `--pg`/`UPAQ_PG_DSN` so every reading is written.
3. Point Prometheus at `/metrics` and/or Grafana at the database.

A full server deployment (dedicated host, systemd unit, Prometheus scrape job,
Grafana datasource + dashboard) is documented in **`deploy/DEPLOY.md`**, with the
artifacts in `deploy/` and `db/`.

## How it works

`adopt` (over the sensor's HTTPS API on :8080) tells the sensor which console to
use; the sensor then opens a `wss://<console>:7442/` connection (subprotocol
`ucp4`, custom "binme" framing) and streams `readingEvent` messages, which this
tool decodes, displays, and optionally stores. Protocol details: `docs/PROTOCOL.md`.

## Layout

- `src/` — the Rust tool (`client` HTTPS, `proto` framing, `console` WSS server, `model`, `pg` Timescale writer, `mqtt` Home Assistant publisher, `tls`, `http` dashboard).
- `docs/PROTOCOL.md` — reverse-engineered protocol.
- `docs/REFLASH.md` — notes on custom/open firmware (optional; not needed to run the app).
- `db/`, `deploy/` — schema + server deployment kit.
- `firmware/` — analysis notes + `fetch-firmware.sh` (vendor images are not shipped).
- `assets/fonts/` — the dashboard's bundled fonts (compiled into the binary).

## License

[PolyForm Noncommercial 1.0.0](LICENSE). You're free to use, modify and share
it for personal, hobby, research and other non-commercial purposes. Commercial
use, including selling it or building it into a paid product or service,
needs permission from Toasteez Limited.

The bundled fonts in `assets/fonts/`, [Plus Jakarta Sans](https://github.com/tokotype/PlusJakartaSans)
and [JetBrains Mono](https://github.com/JetBrains/JetBrainsMono), are under
the SIL Open Font License 1.1. Their licence texts are alongside them.
