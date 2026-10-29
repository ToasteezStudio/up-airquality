# Deploying up-airquality as a server

Target architecture: the collector runs on a small always-on Linux host, writes
every reading to **TimescaleDB**, serves the dashboard + Prometheus `/metrics`,
and **Grafana** graphs the history. The sensor dials the collector, so the host
must be reachable **from the sensor on tcp/7442**.

Placeholders used throughout:

| Placeholder | Meaning |
|---|---|
| `<SENSOR_IP>` | the UP-AirQuality sensor |
| `<COLLECTOR_IP>` | the host running up-airquality |
| `<DB_HOST>` | the PostgreSQL + TimescaleDB server |
| `<GRAFANA_HOST>` | the host Grafana connects from |

Every artifact referenced below is in this repo (`deploy/`, `db/`).

## 1. Prepare the collector host
Any small Linux machine, VM or container works (1 vCPU, 512 MB–1 GB RAM, 2 GB
disk is plenty). It needs a network path **from the sensor to
`<COLLECTOR_IP>:7442`** and, if you use history, to `<DB_HOST>:5432`.

## 2. Database (on `<DB_HOST>`)
Requires PostgreSQL with the TimescaleDB extension.
```bash
createdb airquality
psql -d airquality -f db/schema.sql      # from this repo
psql -d airquality -c "CREATE ROLE upaq LOGIN PASSWORD '<pw>';
  GRANT CONNECT ON DATABASE airquality TO upaq;
  GRANT INSERT, SELECT ON air_quality TO upaq;"
psql -d airquality -c "CREATE ROLE grafana_ro LOGIN PASSWORD '<pw2>';
  GRANT CONNECT ON DATABASE airquality TO grafana_ro;
  GRANT SELECT ON air_quality, air_quality_5m TO grafana_ro;"
```
Allow `<COLLECTOR_IP>` to connect as `upaq`, and `<GRAFANA_HOST>` as
`grafana_ro`, in `pg_hba.conf`.

## 3. Build the binary
Build a static musl binary on any Linux machine so it runs regardless of the
collector's glibc version:
```bash
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
# artifact: target/x86_64-unknown-linux-musl/release/up-airquality
```
Copy it to the collector:
```bash
scp target/x86_64-unknown-linux-musl/release/up-airquality \
    root@<COLLECTOR_IP>:/opt/up-airquality/up-airquality
ssh root@<COLLECTOR_IP> chmod +x /opt/up-airquality/up-airquality
```
To upgrade a running service, copy to `up-airquality.new` and `mv -f` it over
the old binary (overwriting a running executable in place fails with
`ETXTBSY`), then restart the service.

## 4. systemd service (on the collector)
Copy `deploy/up-airquality.service` to `/etc/systemd/system/` and replace
`<SENSOR_IP>` in its `ExecStart` line, so the console only accepts your
sensor. For several sensors, give a comma-separated list
(`--host 10.0.30.11,10.0.30.12`).
Put the DSN in a root-only env file instead of the unit. Add `sslmode=require`
to it, plus `UPAQ_PG_CA=/path/to/ca.pem` for a self-signed server, if the
database link should be encrypted:
```bash
install -m 600 /dev/null /etc/up-airquality.env
echo 'UPAQ_PG_DSN=host=<DB_HOST> port=5432 user=upaq password=<pw> dbname=airquality' \
  > /etc/up-airquality.env
systemctl daemon-reload && systemctl enable --now up-airquality
systemctl status up-airquality
journalctl -u up-airquality -f     # expect "console listening" + "persisting readings"
```
To run without a database, skip the env file and remove `EnvironmentFile=`
from the unit.

To publish to Home Assistant as well, add the broker to the same env file:
```bash
echo 'UPAQ_MQTT_URL=mqtt://<user>:<password>@<BROKER_HOST>:1883' >> /etc/up-airquality.env
systemctl restart up-airquality   # expect "connected to MQTT broker"
```
The collector must be able to reach the broker on tcp/1883. MQTT over TLS
isn't supported yet, so keep that path on a trusted network.

## 5. Firewall: let the sensor dial the collector
If the sensor and the collector are on different VLANs/subnets, add a narrow
allow rule: source `<SENSOR_IP>` → destination `<COLLECTOR_IP>`, **tcp/7442**.
Make sure return traffic is allowed too. Some firewalls only auto-allow
ESTABLISHED replies and can drop the handshake on a dial-back connection.

## 6. Adopt the sensor to the collector
Either run `up-airquality --host <SENSOR_IP> run --console-ip <COLLECTOR_IP>`
once, or send the adopt request directly:
```bash
curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{"username":"ubnt","password":"ubnt","protocol":"wss","hosts":["<COLLECTOR_IP>:7442"],"token":"'"$(uuidgen)"'"}' \
  https://<SENSOR_IP>:8080/api/adopt
```
(`ubnt`/`ubnt` are the vendor's pre-adoption defaults.) To move the sensor to a
different collector later, factory-reset it and adopt again.

## 7. Prometheus
Add `deploy/prometheus-scrape.yml` (with `<COLLECTOR_IP>` filled in) to
`scrape_configs` and reload Prometheus. Example alert:
`upaq_co2_ppm > 1200 for 10m`.

## 8. Grafana
Add a **PostgreSQL** datasource: host `<DB_HOST>:5432`, database `airquality`,
user `grafana_ro`, with TLS set to match your server. Import
`deploy/grafana-dashboard.json` and pick that datasource for `DS_TIMESCALE`.

## 9. Verify
```bash
psql -d airquality -c "SELECT count(*), max(ts) FROM air_quality;"   # climbing
curl -s http://<COLLECTOR_IP>:8099/readings.json                     # live
```
Dashboard: `http://<COLLECTOR_IP>:8099/`. History: the Toasteez Air dashboard
in Grafana.
