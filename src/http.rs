//! Tiny read-only HTTP server for the dashboard, `/readings.json`, and
//! Prometheus `/metrics`. GET-only, one response per connection (Connection:
//! close) — enough for a trusted-LAN status page, no framework needed.
//!
//! The dashboard uses the Toasteez theme (dark, orange accent; Plus Jakarta
//! Sans + JetBrains Mono, bundled in the binary and served from `/fonts/` —
//! nothing is fetched from third parties) and annotates each metric with what
//! it means, a healthy range, and a live Good/Moderate/Poor classification.

use crate::model::Reading;
use crate::state::{now_ms, Metrics, Sensor, Snapshot};
use anyhow::{Context, Result};
use std::borrow::Cow;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;

/// Concurrent HTTP connections; extra ones are dropped, not queued.
const MAX_CONNS: usize = 64;
/// A client must send its request, and accept the response, within this.
const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// Locks the dashboard to its own origin: inline script/style only, fonts and
/// `fetch` from itself, no framing.
const CSP: &str = "default-src 'none'; script-src 'unsafe-inline'; \
    style-src 'unsafe-inline'; font-src 'self'; connect-src 'self'; img-src 'self'; \
    frame-ancestors 'none'; base-uri 'none'; form-action 'none'";

/// Brand fonts (Latin subset, variable weight), SIL OFL 1.1 — see
/// `assets/fonts/*-OFL.txt`.
const FONT_SANS: &[u8] = include_bytes!("../assets/fonts/plus-jakarta-sans.woff2");
const FONT_MONO: &[u8] = include_bytes!("../assets/fonts/jetbrains-mono.woff2");
const WOFF2: &str = "font/woff2";

pub async fn serve(state: Arc<Metrics>, port: u16) -> Result<()> {
    let listener = TcpListener::bind(("0.0.0.0", port))
        .await
        .with_context(|| format!("bind http :{port}"))?;
    tracing::info!(
        port,
        "dashboard: http://localhost:{port}/  (metrics at /metrics)"
    );
    let slots = Arc::new(Semaphore::new(MAX_CONNS));
    loop {
        let (sock, peer) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                // e.g. EMFILE: back off and keep serving rather than exit.
                tracing::warn!(error = %e, "http accept failed");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let Ok(permit) = slots.clone().try_acquire_owned() else {
            tracing::debug!(%peer, "too many http connections; dropping");
            continue;
        };
        let state = state.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let _ = tokio::time::timeout(IO_TIMEOUT, respond(sock, &state)).await;
        });
    }
}

async fn respond(mut sock: TcpStream, state: &Metrics) {
    let mut buf = [0u8; 2048];
    let n = sock.read(&mut buf).await.unwrap_or(0);
    let req = String::from_utf8_lossy(&buf[..n]);
    let path = req.split_whitespace().nth(1).unwrap_or("/");
    let (ct, body) = render(&state.snapshot(), path);
    let head = response_head(ct, body.len());
    let _ = sock.write_all(head.as_bytes()).await;
    let _ = sock.write_all(&body).await;
    let _ = sock.shutdown().await;
}

fn response_head(ct: &str, len: usize) -> String {
    // Fonts are static and safe to cache; everything else is live data.
    let cache = if ct == WOFF2 {
        "public, max-age=86400"
    } else {
        "no-store"
    };
    let mut head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {ct}\r\nContent-Length: {len}\r\n\
         Connection: close\r\nCache-Control: {cache}\r\nX-Content-Type-Options: nosniff\r\n"
    );
    if ct.starts_with("text/html") {
        head.push_str(&format!("Content-Security-Policy: {CSP}\r\n"));
    }
    head.push_str("\r\n");
    head
}

fn render(snap: &Snapshot, path: &str) -> (&'static str, Cow<'static, [u8]>) {
    let text = |s: String| Cow::Owned(s.into_bytes());
    match path.split('?').next().unwrap_or("/") {
        "/metrics" => ("text/plain; version=0.0.4", text(metrics(snap))),
        "/readings.json" => ("application/json", text(readings_json(snap))),
        "/healthz" => ("text/plain", Cow::Borrowed(b"ok\n")),
        "/fonts/plus-jakarta-sans.woff2" => (WOFF2, Cow::Borrowed(FONT_SANS)),
        "/fonts/jetbrains-mono.woff2" => (WOFF2, Cow::Borrowed(FONT_MONO)),
        _ => ("text/html; charset=utf-8", text(dashboard(snap))),
    }
}

fn age_secs(updated_ms: u64) -> Option<u64> {
    if updated_ms == 0 {
        None
    } else {
        Some(now_ms().saturating_sub(updated_ms) / 1000)
    }
}

fn sensor_json(id: &str, s: &Sensor) -> serde_json::Value {
    let reading = s
        .last
        .as_ref()
        .and_then(|r| serde_json::to_value(r).ok())
        .unwrap_or(serde_json::Value::Null);
    serde_json::json!({
        "id": id,
        "reading": reading,
        "updated_ms": s.updated_ms,
        "age_secs": age_secs(s.updated_ms),
        "count": s.count,
        "source": s.peer,
    })
}

/// `sensors` lists every sensor. The top-level fields mirror the most
/// recently updated one, so single-sensor clients keep working.
fn readings_json(snap: &Snapshot) -> String {
    let mut root = match snap.latest() {
        Some((id, s)) => sensor_json(id, s),
        None => serde_json::json!({
            "id": null, "reading": null, "updated_ms": 0,
            "age_secs": null, "count": 0, "source": null,
        }),
    };
    root["sensors"] = snap
        .sensors
        .iter()
        .map(|(id, s)| sensor_json(id, s))
        .collect();
    root.to_string()
}

type Getter = fn(&Reading) -> Option<f64>;

/// Prometheus gauges: name, help, field. One series per sensor.
static GAUGES: [(&str, &str, Getter); 12] = [
    ("upaq_aqi", "Air quality index", |r| r.aqi),
    ("upaq_temperature_celsius", "Temperature C", |r| {
        r.temperature
    }),
    ("upaq_humidity_percent", "Relative humidity %", |r| {
        r.humidity
    }),
    ("upaq_co2_ppm", "CO2 ppm", |r| r.co2),
    ("upaq_pm1p0_ugm3", "PM1.0 ug/m3", |r| r.pm1p0),
    ("upaq_pm2p5_ugm3", "PM2.5 ug/m3", |r| r.pm2p5),
    ("upaq_pm4p0_ugm3", "PM4.0 ug/m3", |r| r.pm4p0),
    ("upaq_pm10p0_ugm3", "PM10 ug/m3", |r| r.pm10p0),
    ("upaq_voc_index", "VOC index", |r| r.voc),
    ("upaq_nox_index", "NOx index", |r| r.nox),
    ("upaq_tvoc", "Total VOC", |r| r.tvoc),
    ("upaq_vape_index", "Vape index", |r| r.vape),
];

/// Escape a Prometheus label value.
fn label(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// Every series carries `sensor="<id>"` (the sensor's MAC, or its address).
fn metrics(snap: &Snapshot) -> String {
    let mut s = String::new();
    for (name, help, get) in &GAUGES {
        let series: Vec<(&String, f64)> = snap
            .sensors
            .iter()
            .filter_map(|(id, sn)| sn.last.as_ref().and_then(get).map(|v| (id, v)))
            .collect();
        if series.is_empty() {
            continue;
        }
        s.push_str(&format!("# HELP {name} {help}\n# TYPE {name} gauge\n"));
        for (id, v) in series {
            s.push_str(&format!("{name}{{sensor=\"{}\"}} {v}\n", label(id)));
        }
    }
    s.push_str("# HELP upaq_sensors Sensors seen since start\n# TYPE upaq_sensors gauge\n");
    s.push_str(&format!("upaq_sensors {}\n", snap.sensors.len()));
    s.push_str(
        "# HELP upaq_readings_total Readings received\n# TYPE upaq_readings_total counter\n",
    );
    for (id, sn) in &snap.sensors {
        s.push_str(&format!(
            "upaq_readings_total{{sensor=\"{}\"}} {}\n",
            label(id),
            sn.count
        ));
    }
    s.push_str("# HELP upaq_last_update_timestamp_seconds Unix time of last reading\n# TYPE upaq_last_update_timestamp_seconds gauge\n");
    for (id, sn) in &snap.sensors {
        s.push_str(&format!(
            "upaq_last_update_timestamp_seconds{{sensor=\"{}\"}} {}\n",
            label(id),
            sn.updated_ms / 1000
        ));
    }
    s
}

fn dashboard(snap: &Snapshot) -> String {
    // Raw template + a single placeholder for the initial JSON — avoids the
    // brace-escaping hazard of format! across a large JS/CSS blob. `<` is
    // escaped so the JSON can never close the <script> element.
    DASHBOARD_HTML.replace("__INITIAL__", &readings_json(snap).replace('<', "\\u003c"))
}

const DASHBOARD_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Toasteez · Air Quality</title>
<style>
  @font-face { font-family:'Plus Jakarta Sans'; src:url('/fonts/plus-jakarta-sans.woff2') format('woff2'); font-weight:200 800; font-style:normal; font-display:swap; }
  @font-face { font-family:'JetBrains Mono'; src:url('/fonts/jetbrains-mono.woff2') format('woff2'); font-weight:100 800; font-style:normal; font-display:swap; }
  :root {
    --bg:#0a0a0b; --surface:#18181b; --elevated:#1f1f23;
    --border:#27272a; --border-hover:#71717a;
    --fg:#fafafa; --fg2:#d4d4d8; --muted:#a1a1aa;
    --accent:#f97316; --accent-dark:#c2410c; --accent-hover:#ea580c;
    --good:#22c55e; --warn:#fbbf24; --bad:#ef4444;
    --font:'Plus Jakarta Sans',-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,sans-serif;
    --mono:'JetBrains Mono','Fira Code',Consolas,monospace;
    --radius:12px;
  }
  * { box-sizing:border-box; }
  body { margin:0; background:var(--bg); color:var(--fg); font-family:var(--font); -webkit-font-smoothing:antialiased; }
  .wrap { max-width:960px; margin:0 auto; padding:28px 16px 48px; }
  header { display:flex; align-items:center; gap:12px; margin-bottom:4px; }
  header svg { width:34px; height:34px; filter:drop-shadow(0 0 10px rgba(249,115,22,.35)); }
  .brand { font-weight:800; font-size:22px; letter-spacing:-.01em; }
  .brand .accent { color:var(--accent); }
  .tag { font-size:13px; color:var(--muted); font-family:var(--mono); border:1px solid var(--border); border-radius:999px; padding:3px 10px; margin-left:4px; }
  .status { display:flex; align-items:center; gap:8px; color:var(--muted); font-size:13px; margin:14px 0 22px; font-family:var(--mono); }
  .status .dot { width:9px; height:9px; border-radius:50%; background:var(--muted); box-shadow:0 0 0 0 rgba(249,115,22,.5); }
  .status.live .dot { background:var(--good); animation:pulse 2s infinite; }
  .status.stale .dot { background:var(--bad); }
  @keyframes pulse { 0%{box-shadow:0 0 0 0 rgba(34,197,94,.5);} 70%{box-shadow:0 0 0 7px rgba(34,197,94,0);} 100%{box-shadow:0 0 0 0 rgba(34,197,94,0);} }
  .grid { display:grid; grid-template-columns:repeat(auto-fill,minmax(228px,1fr)); gap:14px; }
  .card { position:relative; background:var(--surface); border:1px solid var(--border); border-left:3px solid var(--status,var(--border)); border-radius:var(--radius); padding:16px 18px; transition:border-color 150ms ease, transform 150ms ease; }
  .card:hover { border-color:var(--border-hover); border-left-color:var(--status,var(--border-hover)); transform:translateY(-1px); }
  .card.empty { opacity:.45; }
  .card__top { display:flex; align-items:center; justify-content:space-between; gap:8px; }
  .card__label { font-size:13px; font-weight:600; color:var(--fg2); letter-spacing:.01em; }
  .pill { font-size:11px; font-weight:700; color:var(--p,var(--muted)); background:color-mix(in srgb, var(--p,#888) 15%, transparent); border:1px solid color-mix(in srgb, var(--p,#888) 35%, transparent); border-radius:999px; padding:2px 9px; white-space:nowrap; }
  .card__value { font-size:32px; font-weight:700; margin:10px 0 2px; font-variant-numeric:tabular-nums; letter-spacing:-.02em; }
  .card__value .unit { font-size:14px; font-weight:500; color:var(--muted); margin-left:4px; }
  .card__healthy { font-family:var(--mono); font-size:11.5px; color:var(--accent); margin-bottom:8px; }
  .card__desc { font-size:12.5px; line-height:1.45; color:var(--muted); }
  footer { margin-top:26px; color:var(--muted); font-size:12px; font-family:var(--mono); }
  footer a { color:var(--accent); text-decoration:none; }
  footer a:hover { text-decoration:underline; }
  .sensor + .sensor { margin-top:28px; }
  .sensor__head { display:flex; align-items:center; flex-wrap:wrap; gap:10px; margin:0 0 12px; }
  .sensor__name { font-size:15px; font-weight:700; color:var(--fg2); font-family:var(--mono); }
  .sensor__age { font-size:12px; color:var(--muted); font-family:var(--mono); }
  .copyright { margin-top:14px; padding-top:12px; border-top:1px solid var(--border); color:var(--muted); font-size:11px; line-height:1.5; }
</style>
</head>
<body>
<div class="wrap">
  <header>
    <svg viewBox="4 2 54 54" xmlns="http://www.w3.org/2000/svg" aria-label="Toasteez">
      <path d="M11 45 V25 C11 22.5 8 21.5 8 17 C8 10.5 15 6 24.5 6 C27.5 6 29.8 6.6 31.5 7.8 A 5.6 5.6 0 0 0 38.8 12.5 A 5.6 5.6 0 0 0 44 20 A 5.6 5.6 0 0 0 45 29 V45 A4 4 0 0 1 41 49 H15 A4 4 0 0 1 11 45 Z" fill="#f97316" stroke="#ea580c" stroke-width="2" stroke-linejoin="round"/>
      <rect x="48.5" y="23" width="4.4" height="4.4" rx="1" fill="#f97316" transform="rotate(22 50.7 25.2)"/>
      <rect x="51.5" y="32" width="3.2" height="3.2" rx="0.8" fill="#f97316" transform="rotate(-18 53.1 33.6)"/>
      <rect x="48.5" y="39" width="2.6" height="2.6" rx="0.7" fill="#ea580c" transform="rotate(35 49.8 40.3)"/>
      <g fill="#0a0a0b" stroke="#0a0a0b" stroke-linecap="round" stroke-linejoin="round">
        <path d="M30.5 25.5 C34 25.5 36 27 36 29.5 C36 32 34 33.5 31 33.5" fill="none" stroke-width="2.4"/>
        <path d="M16.5 24 H31.5 V30.5 C31.5 34.6 28.1 38 24 38 C19.9 38 16.5 34.6 16.5 30.5 Z"/>
        <rect x="14" y="40.2" width="20" height="3.2" rx="1.6"/>
        <path d="M20.5 20.5 C22.1 19 22.1 17.5 20.5 16" fill="none" stroke-width="2.2"/>
        <path d="M26.5 20.5 C28.1 19 28.1 17.5 26.5 16" fill="none" stroke-width="2.2"/>
      </g>
    </svg>
    <span class="brand"><span class="accent">Toasteez</span> Air</span>
    <span class="tag">UP-AirQuality</span>
  </header>
  <div class="status" id="status"><span class="dot"></span><span id="statustext">connecting…</span></div>
  <div id="sensors"></div>
  <footer id="foot"></footer>
  <div class="copyright">&copy; 2026 Toasteez Limited. Company No. 07656957.</div>
</div>
<script>
// key, label, unit, healthy-range text, description, classifier -> good|warn|bad
const METRICS = [
  ["aqi","Air Quality Index","","0–50 good","Overall score the sensor derives from gases and particulates. Higher = worse air.", v=> v<=50?"good":v<=100?"warn":"bad"],
  ["co2","CO₂","ppm","< 800 ppm","Carbon dioxide — a proxy for ventilation. Climbs with people and poor airflow; high levels cause drowsiness and poor focus.", v=> v<800?"good":v<=1200?"warn":"bad"],
  ["pm2p5","PM2.5","µg/m³","< 12 (24h)","Fine particles ≤2.5µm from smoke, cooking and dust. Reach deep into the lungs — the headline health pollutant.", v=> v<=12?"good":v<=35?"warn":"bad"],
  ["pm10p0","PM10","µg/m³","< 54 (24h)","Coarser particles ≤10µm such as dust and pollen. Irritate airways.", v=> v<=54?"good":v<=154?"warn":"bad"],
  ["pm1p0","PM1.0","µg/m³","as low as poss.","Ultrafine particles ≤1µm from combustion and smoke. No formal limit — watch the trend.", v=> v<=10?"good":v<=25?"warn":"bad"],
  ["pm4p0","PM4.0","µg/m³","as low as poss.","Particles ≤4µm. No formal limit; track alongside PM2.5/PM10.", v=> v<=25?"good":v<=50?"warn":"bad"],
  ["voc","VOC Index","","≈ 100 (≤150)","Sensirion VOC index — solvents, cleaning products, paint, off-gassing — scaled to the last 24h. 100 = typical; spikes mean something is off-gassing.", v=> v<=150?"good":v<=250?"warn":"bad"],
  ["nox","NOx Index","","≈ 1 (≤20)","Sensirion NOx index — nitrogen oxides from combustion (gas hobs, traffic). 1 = baseline; a jump means a combustion source.", v=> v<=20?"good":v<=150?"warn":"bad"],
  ["tvoc","Total VOC","ppb","< 220 ppb","Estimated total VOC concentration. Lower is better; rises with cleaning, cooking and aerosols.", v=> v<=220?"good":v<=660?"warn":"bad"],
  ["temperature","Temperature","°C","20–24 °C","Ambient temperature. 20–24°C is the usual indoor comfort band.", v=> (v>=20&&v<=24)?"good":(v>=18&&v<=26)?"warn":"bad"],
  ["humidity","Humidity","%","40–60 %","Relative humidity. Below 30% dries airways; above 60% invites mould and dust mites.", v=> (v>=40&&v<=60)?"good":(v>=30&&v<=70)?"warn":"bad"],
  ["vape","Vape","","0","Vaping / aerosol detection index. 0 means nothing detected.", v=> v<=1?"good":v<=20?"warn":"bad"],
];
const STATUS = { good:["Good","var(--good)"], warn:["Moderate","var(--warn)"], bad:["Poor","var(--bad)"] };
function fmt(v){ return (v==null)?"–":(Math.round(v*10)/10); }
function esc(s){ return String(s).replace(/[&<>]/g, c=>({"&":"&amp;","<":"&lt;",">":"&gt;"}[c])); }
const LIVE_SECS = 30;
function ago(age){ return age<LIVE_SECS ? `live · updated ${age}s ago` : `stale · last reading ${age}s ago`; }
function grid(r){
  const grid = document.createElement("div");
  grid.className = "grid";
  for (const [k,label,unit,healthy,desc,band] of METRICS){
    const v = r[k];
    const card = document.createElement("div");
    card.className = "card" + (v==null?" empty":"");
    let pill = "";
    if (v!=null){
      const [txt,col] = STATUS[band(v)] || STATUS.good;
      card.style.setProperty("--status", col);
      pill = `<span class="pill" style="--p:${col}">${txt}</span>`;
    }
    card.innerHTML =
      `<div class="card__top"><span class="card__label">${esc(label)}</span>${pill}</div>`+
      `<div class="card__value">${fmt(v)}<span class="unit">${esc(unit)}</span></div>`+
      `<div class="card__healthy">Healthy: ${esc(healthy)}</div>`+
      `<div class="card__desc">${esc(desc)}</div>`;
    grid.appendChild(card);
  }
  return grid;
}
function render(d){
  const list = d.sensors || [];
  const root = document.getElementById("sensors");
  root.innerHTML = "";
  if (!list.length) root.appendChild(grid({}));
  for (const s of list){
    const sec = document.createElement("section");
    sec.className = "sensor";
    if (list.length > 1){
      const head = document.createElement("div");
      head.className = "sensor__head";
      head.innerHTML =
        `<span class="sensor__name">${esc(s.id)}</span>`+
        `<span class="tag">${esc(s.source||"?")}</span>`+
        `<span class="sensor__age">${s.age_secs==null?"":esc(ago(s.age_secs))}</span>`;
      sec.appendChild(head);
    }
    sec.appendChild(grid(s.reading || {}));
    root.appendChild(sec);
  }
  const ages = list.map(s=>s.age_secs).filter(a=>a!=null);
  const live = ages.filter(a=>a<LIVE_SECS).length;
  const st = document.getElementById("status");
  st.className = "status " + (!ages.length ? "" : (live ? "live" : "stale"));
  document.getElementById("statustext").textContent =
    !ages.length      ? "no readings yet — waiting for a sensor to connect"
    : list.length===1 ? ago(ages[0])
                      : `${live} of ${list.length} sensors live`;
  const total = list.reduce((n,s)=>n+(s.count||0), 0);
  const src = list.length===1 ? ` · source ${esc(list[0].source||"?")}` : ` · ${list.length} sensors`;
  document.getElementById("foot").innerHTML =
    `${total} readings${list.length?src:""} · `+
    `<a href="/metrics">/metrics</a> · <a href="/readings.json">/readings.json</a>`;
}
async function tick(){
  try { const r = await fetch("/readings.json",{cache:"no-store"}); render(await r.json()); }
  catch(e){ document.getElementById("statustext").textContent = "dashboard unreachable"; }
}
render(__INITIAL__);
setInterval(tick, 3000); tick();
</script>
</body>
</html>
"##;

#[cfg(test)]
mod tests {
    use super::*;

    /// A snapshot with one sensor (no readings yet) per `(id, peer)`.
    fn snap_of(sensors: &[(&str, &str)]) -> Snapshot {
        let mut s = Snapshot::default();
        for (id, peer) in sensors {
            s.sensors.insert(
                id.to_string(),
                Sensor {
                    peer: Some(peer.to_string()),
                    ..Default::default()
                },
            );
        }
        s
    }

    fn snap(peer: Option<&str>) -> Snapshot {
        match peer {
            Some(p) => snap_of(&[("02AABBCCDDEE", p)]),
            None => Snapshot::default(),
        }
    }

    fn with_reading(mut s: Snapshot, id: &str, co2: f64, updated_ms: u64) -> Snapshot {
        let sn = s.sensors.get_mut(id).unwrap();
        sn.last = Some(Reading {
            co2: Some(co2),
            ..Default::default()
        });
        sn.updated_ms = updated_ms;
        sn.count += 1;
        s
    }

    #[test]
    fn readings_json_lists_sensors_and_keeps_legacy_fields() {
        let s = snap_of(&[("AAAAAAAAAAAA", "10.0.0.1"), ("BBBBBBBBBBBB", "10.0.0.2")]);
        let s = with_reading(s, "AAAAAAAAAAAA", 500.0, 1_000);
        let s = with_reading(s, "BBBBBBBBBBBB", 900.0, 2_000);
        let v: serde_json::Value = serde_json::from_str(&readings_json(&s)).unwrap();
        assert_eq!(v["sensors"].as_array().unwrap().len(), 2);
        assert_eq!(v["sensors"][0]["id"], "AAAAAAAAAAAA");
        // Top level mirrors the most recently updated sensor.
        assert_eq!(v["id"], "BBBBBBBBBBBB");
        assert_eq!(v["reading"]["co2"], 900.0);
        assert_eq!(v["source"], "10.0.0.2");

        let empty: serde_json::Value =
            serde_json::from_str(&readings_json(&Snapshot::default())).unwrap();
        assert!(empty["reading"].is_null());
        assert_eq!(empty["count"], 0);
        assert_eq!(empty["sensors"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn metrics_label_each_sensor() {
        let s = snap_of(&[("AAAAAAAAAAAA", "10.0.0.1"), ("BBBBBBBBBBBB", "10.0.0.2")]);
        let s = with_reading(s, "AAAAAAAAAAAA", 500.0, 1_000);
        let s = with_reading(s, "BBBBBBBBBBBB", 900.0, 2_000);
        let m = metrics(&s);
        assert_eq!(m.matches("# TYPE upaq_co2_ppm gauge").count(), 1);
        assert!(m.contains("upaq_co2_ppm{sensor=\"AAAAAAAAAAAA\"} 500"));
        assert!(m.contains("upaq_co2_ppm{sensor=\"BBBBBBBBBBBB\"} 900"));
        assert!(m.contains("upaq_sensors 2"));
        assert!(m.contains("upaq_readings_total{sensor=\"BBBBBBBBBBBB\"} 1"));
        assert!(
            !m.contains("upaq_aqi"),
            "no series for fields no sensor sent"
        );
    }

    #[test]
    fn hardening_headers_and_no_cors() {
        let html = response_head("text/html; charset=utf-8", 10);
        assert!(!html.contains("Access-Control-Allow-Origin"));
        assert!(html.contains("X-Content-Type-Options: nosniff"));
        assert!(html.contains("Content-Security-Policy: "));
        assert!(html.ends_with("\r\n\r\n"));
        let json = response_head("application/json", 2);
        assert!(!json.contains("Access-Control-Allow-Origin"));
        assert!(!json.contains("Content-Security-Policy"));
    }

    #[test]
    fn dashboard_fetches_nothing_third_party() {
        let html = dashboard(&snap(None));
        assert!(!html.contains("fonts.googleapis.com"));
        assert!(!html.contains("__INITIAL__"));
    }

    #[test]
    fn serves_bundled_fonts() {
        for path in [
            "/fonts/plus-jakarta-sans.woff2",
            "/fonts/jetbrains-mono.woff2",
        ] {
            let (ct, body) = render(&snap(None), path);
            assert_eq!(ct, WOFF2);
            assert!(body.starts_with(b"wOF2"), "{path}");
        }
        assert!(response_head(WOFF2, 1).contains("Cache-Control: public"));
        assert!(CSP.contains("font-src 'self'"));
        let html = dashboard(&snap(None));
        assert!(html.contains("url('/fonts/plus-jakarta-sans.woff2')"));
        assert!(html.contains("url('/fonts/jetbrains-mono.woff2')"));
    }

    #[test]
    fn embedded_json_cannot_close_script() {
        let html = dashboard(&snap(Some("</script><script>alert(1)</script>")));
        assert!(!html.contains("</script><script>alert(1)"));
        assert!(html.contains("\\u003c/script>"));
    }
}
