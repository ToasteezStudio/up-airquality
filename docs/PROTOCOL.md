# UP-AirQuality — reverse-engineered protocol

Target device: Ubiquiti **UniFi Protect UP-AirQuality** sensor, PoE, ESP32-based.
Unit tested: model string `UP-AirQuality`, firmware `UPAQ.esp32.v1.0.12.0.f93255.260605.1107`. Sensor chip:
Sensirion **SEN66**.

Everything here was derived from (a) live probing of the unit and (b) `strings`
on the public, unencrypted firmware image (see `../firmware/`). Items marked
**(confirmed live)** were observed against the device; the rest are from firmware
analysis and may need live confirmation.

## 1. Pre-adoption control API — HTTPS on tcp/8080

- TLS 1.3, self-signed cert generated on-device: `CN=ui.com` (secp256r1),
  valid 2025→2100. No client cert requested. **(confirmed live)**
- Auth is JSON body fields `username` / `password`, verified by `json_auth_verify`.
  HTTP Basic is also parsed but the JSON body is the working path. The only
  built-in user is `ubnt`; passwords are stored MD5 in NVS (`user_pswd_%02d`).
- Vendor default `ubnt`/`ubnt` works. **(confirmed live)**

Error shapes seen: empty body → `400 Missing Content-Type`; bad/no creds →
`401 Unauthorized`; unknown path → `404 Nothing matches the given URI`; wrong
method → `405`.

### Routes (from firmware)
| Route | Method | Purpose |
|---|---|---|
| `/api/info` | POST | Identity. **Confirmed live** → see below. |
| `/api/adopt` | POST | Write adoption config, start dial-back. |
| `/api/adopt/layer3` | POST | Same, host inferred from caller IP. |
| `/api/locate` | POST | Blink status LED. |
| `/api/support` | POST | Support/debug bundle. |
| `/api/ota` | — | OTA entry point (see REFLASH.md). |

### `POST /api/info` (confirmed live)
Request: `{"username":"ubnt","password":"ubnt"}`
Response `200`:
```json
{
  "type": "UP-AirQuality",
  "version": "UPAQ.esp32.v1.0.12.0.f93255.260605.1107",
  "sysid": "aea6",
  "mac": "02AABBCCDDEE",
  "uptime": 111417,
  "featureFlags": { "hasLedStatus": true }
}
```

### `POST /api/adopt` (fields confirmed; exact minimal body inferred)
The device parses and persists (to `/storage/ucpv4c.cfg` + `/storage/adopt.json`):
`protocol`, `host`, `hosts[]`, `token`, `key`, `nvr`, `consoleId`, `controller`,
`controllerId`, `fingerprint`, `timezone`, `name`, `adoptionType`, `mode`,
`wifi{enabled,auth,ssid,key}`.

`/api/adopt/layer3` carries a template `{"protocol":"wss","host":"%r:7442"}` —
`%r` = caller IP — so it builds the dial-back host from whoever called it.

A minimal adopt body this client sends:
```json
{ "username":"ubnt", "password":"ubnt",
  "protocol":"wss", "hosts":["<console-ip>:7442"],
  "token":"<uuid>", "fingerprint":"<optional sha256 of console cert>" }
```

## 2. Connect-back — UCP4 over WSS (tcp/7442)

After adoption the device dials `wss://<host>:7442/` (firmware format `%s://%s/`).
- Subprotocol header: `sec-websocket-protocol: ucp4`
- Identity headers: `x-adopted`, `x-sysid` (`%04x`), `x-type`, `x-ident` (MAC),
  `x-ip`, `x-region`, `x-guid`, and **one of** `x-token`+`x-mode:0` (first
  adoption) or `x-key`+`x-mode:1` (adopted reconnect).
- The device loads `cert.pem`/`key.pem` as a client cert → **mutual TLS is
  likely**. `fingerprint` from adopt probably pins the console cert; an empty
  value may skip the check (unconfirmed). Our server does not *request* a client
  cert, so the handshake should still complete.

### Framing ("binme") — implemented in `src/proto.rs`
One logical message = two concatenated frames in a single WS binary message:
`HEAD` (frame_type 1) + `BODY` (frame_type 2). Each frame:
```
offset 0: frame_type     (1=head, 2=body)
offset 1: payload_format  (1=JSON)
offset 2: deflated        (0=raw, 1=zlib)
offset 3: reserved (0)
offset 4..8: u32 BE payload length
offset 8..: payload (zlib-compressed if deflated==1)
```
Matches hjdhjd/unifi-protect's realtime packet decoder.

Head JSON: `{"timestamp":ms,"id":"<uuid>","type":"request|response|event","action":"…"}`.
Errors use `errorCode`/`error`; events carry `name`.

### Messages
Device → console:
- request `timeSync` body `{"t0":ms}` → expects `{t1,t2}` back (NTP-style). We answer.
- event **`readingEvent`** body:
  `{aqi, vape, tvoc, pm1p0, pm2p5, pm4p0, pm10p0, humidity, temperature, voc, nox, co2}`
- event `alertEvent`, `vapeSensitivityEvent {vape}`.

Console → device actions: `setConfig`, `setRingLedColor`, `getInfo`, `setInfo`,
`networkStatus`, `locate`, `setIndicatorLed`, `updateFirmware`, `factoryReset`,
`getSupportInfo`, `liveLogging`, `sshService`, `changeUserPassword`,
`changeLightSettings`.

`setConfig` keys: `ringLedMetric`, `ringLedBrightness`, `readingInterval`,
`alertInterval`, `vapeSensitivityEventEnabled/Threshold`,
`nightModeEnabled/StartTime/EndTime/Brightness`, `timezoneMinute`, `timezoneDst`,
and per-metric `<m>AlertEnabled`, `<m>Min`, `<m>Max`.

## 3. Reading schema & units
Console-side (`hjdhjd/unifi-protect` `airQuality`), units from retail specs:
CO2 ppm (≤40000), PM µg/m³ (≤1000), VOC index 1–500, NOx index 1–500, vape index
0–100, temperature °C, humidity %RH, AQI integer.

## 4. Confirmed live 2026-10-28 (end-to-end adopt + readings)
Adopted the real unit (console on a different VLAN, with a temporary firewall
allow from the sensor to the console's tcp/7442) and
received a live `readingEvent` stream. Resolved:
- **Mutual TLS NOT required.** Server used `with_no_client_auth()` and the device
  completed the handshake.
- **No `fingerprint` pinning needed.** We sent no `fingerprint` in `/api/adopt`
  and the device connected fine.
- **`timeSync`**: device sends it right after connect; our response
  `{t0,t1,t2}` (echoing id, `type:"response"`, `action:"timeSync"`) was accepted
  (no re-request loop).
- **First adopt** arrives with `x-mode:0` and `x-token` = a device-chosen UUID
  (not the adopt `token` we sent); reconnects are expected to use `x-mode:1`+`x-key`.
- Dial-back headers seen: `x-adopted:false`, `x-sysid:aea6`, `x-type`,
  `x-ident`(MAC), `x-ip`, `x-region:EU`, `x-guid`, `x-token`, `x-mode`.
- `readingEvent` fields all present and sane: `aqi,temperature,humidity,co2,`
  `pm2p5,pm10p0,voc,nox,vape` (PM1/PM4/tvoc may be omitted). Emitted every few
  seconds.

### Still open
- `/api/adopt` vs `/api/adopt/layer3` body differences (we used `/api/adopt`).
- Reconnect path (`x-mode:1`/`x-key`) not yet exercised.
- Console→device actions (`setConfig`, `locate`, `factoryReset`, …) not yet sent.
