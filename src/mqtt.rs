//! Optional MQTT publishing with Home Assistant MQTT Discovery. Each sensor
//! appears in Home Assistant as a device (keyed by its MAC) with one typed
//! entity per metric, with no YAML.
//!
//! Topics:
//!   up-airquality/status                       online / offline (retained, last will)
//!   up-airquality/<sensor>/state               latest reading as JSON (retained)
//!   <prefix>/device/upaq_<sensor>/config       device discovery (retained)
//!
//! Discovery is re-sent after every (re)connect and whenever Home Assistant
//! announces itself with `online` on `<prefix>/status`. Like the Postgres
//! writer, this runs in its own task and never blocks the sensor connection.
//!
//! Plain `mqtt://` only for now: rumqttc 0.25's TLS support depends on a
//! rustls-webpki release with open advisories (see Cargo.toml).

use crate::model::Reading;
use anyhow::{bail, Context, Result};
use percent_encoding::percent_decode_str;
use rumqttc::{AsyncClient, Event, EventLoop, LastWill, MqttOptions, Packet, QoS};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::sync::mpsc;

/// (sensor id, reading)
pub type Msg = (String, Reading);

/// Root of this program's own topics.
const BASE: &str = "up-airquality";
/// Entities go unavailable in Home Assistant when a sensor sends nothing for
/// this long (it normally reports every few seconds).
const EXPIRE_AFTER_SECS: u64 = 300;
/// Home Assistant writes the micro prefix as Greek mu (U+03BC); the micro
/// sign (U+00B5) is not an accepted unit.
const UGM3: &str = "\u{3bc}g/m\u{b3}";

/// Entities: reading field, name, Home Assistant device class, unit.
const ENTITIES: [(&str, &str, Option<&str>, Option<&str>); 12] = [
    ("aqi", "Air quality index", Some("aqi"), None),
    ("co2", "CO2", Some("carbon_dioxide"), Some("ppm")),
    ("pm1p0", "PM1", Some("pm1"), Some(UGM3)),
    ("pm2p5", "PM2.5", Some("pm25"), Some(UGM3)),
    ("pm4p0", "PM4", Some("pm4"), Some(UGM3)),
    ("pm10p0", "PM10", Some("pm10"), Some(UGM3)),
    (
        "temperature",
        "Temperature",
        Some("temperature"),
        Some("\u{b0}C"),
    ),
    ("humidity", "Humidity", Some("humidity"), Some("%")),
    (
        "tvoc",
        "Total VOC",
        Some("volatile_organic_compounds_parts"),
        Some("ppb"),
    ),
    // Sensirion's VOC/NOx indices and the vape index are unitless scores,
    // not concentrations, so no device class fits them.
    ("voc", "VOC index", None, None),
    ("nox", "NOx index", None, None),
    ("vape", "Vape index", None, None),
];

/// A parsed `mqtt://` broker URL.
#[derive(Debug, PartialEq)]
struct Broker {
    host: String,
    port: u16,
    login: Option<(String, String)>,
}

fn parse_url(raw: &str) -> Result<Broker> {
    let u = url::Url::parse(raw).context("parsing MQTT URL")?;
    match u.scheme() {
        "mqtt" => {}
        "mqtts" => bail!(
            "mqtts:// (MQTT over TLS) is not supported yet; use mqtt:// with a \
             broker password on a network you trust"
        ),
        other => bail!("MQTT URL must start mqtt://, not {other}://"),
    }
    let host = u.host_str().context("MQTT URL has no host")?.to_string();
    let port = u.port().unwrap_or(1883);
    let decode = |s: &str| percent_decode_str(s).decode_utf8().map(|c| c.into_owned());
    let login = if u.username().is_empty() {
        None
    } else {
        Some((
            decode(u.username()).context("MQTT username")?,
            decode(u.password().unwrap_or("")).context("MQTT password")?,
        ))
    };
    Ok(Broker { host, port, login })
}

/// `upaq_<id>` with anything outside `[a-z0-9_]` replaced, as discovery
/// object ids must be `[a-zA-Z0-9_-]`.
fn slug(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

fn state_topic(id: &str) -> String {
    format!("{BASE}/{}/state", slug(id))
}

/// `aa:bb:cc:dd:ee:ff` when the sensor id is a MAC (12 hex digits).
fn mac(id: &str) -> Option<String> {
    (id.len() == 12 && id.chars().all(|c| c.is_ascii_hexdigit())).then(|| {
        id.as_bytes()
            .chunks(2)
            .map(|p| String::from_utf8_lossy(p).to_ascii_lowercase())
            .collect::<Vec<_>>()
            .join(":")
    })
}

/// Device-based discovery message for one sensor: (topic, payload).
fn discovery(prefix: &str, id: &str) -> (String, Value) {
    let object_id = format!("upaq_{}", slug(id));
    let short = if id.len() == 12 { &id[6..] } else { id };
    let mut device = json!({
        "identifiers": [object_id],
        "name": format!("UP-AirQuality {short}"),
        "manufacturer": "Ubiquiti",
        "model": "UP-AirQuality",
    });
    if let Some(mac) = mac(id) {
        device["connections"] = json!([["mac", mac]]);
    }
    let mut components = serde_json::Map::new();
    for (key, name, class, unit) in ENTITIES {
        let mut c = json!({
            "platform": "sensor",
            "unique_id": format!("{object_id}_{key}"),
            "name": name,
            "state_class": "measurement",
            "value_template": format!("{{{{ value_json.{key} }}}}"),
            "expire_after": EXPIRE_AFTER_SECS,
        });
        if let Some(class) = class {
            c["device_class"] = json!(class);
        }
        if let Some(unit) = unit {
            c["unit_of_measurement"] = json!(unit);
        }
        components.insert(key.to_string(), c);
    }
    let payload = json!({
        "device": device,
        "origin": {
            "name": "up-airquality",
            "sw_version": env!("CARGO_PKG_VERSION"),
            "support_url": env!("CARGO_PKG_REPOSITORY"),
        },
        "availability": [{ "topic": format!("{BASE}/status") }],
        "state_topic": state_topic(id),
        "components": components,
    });
    (format!("{prefix}/device/{object_id}/config"), payload)
}

/// Start the publisher. Fails only on a malformed URL; broker outages are
/// retried in the background.
pub fn spawn(url: &str, prefix: String) -> Result<mpsc::Sender<Msg>> {
    let broker = parse_url(url)?;
    let client_id = format!(
        "up-airquality-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    );
    let mut opts = MqttOptions::new(client_id, broker.host.clone(), broker.port);
    opts.set_keep_alive(Duration::from_secs(30));
    // Discovery for one sensor is a few KiB; leave headroom.
    opts.set_max_packet_size(64 * 1024, 64 * 1024);
    opts.set_last_will(LastWill::new(
        format!("{BASE}/status"),
        "offline",
        QoS::AtLeastOnce,
        true,
    ));
    if let Some((user, pass)) = &broker.login {
        opts.set_credentials(user, pass);
    }
    tracing::info!(
        host = %broker.host, port = broker.port, %prefix,
        "publishing to MQTT with Home Assistant discovery"
    );
    let (client, eventloop) = AsyncClient::new(opts, 64);
    let (tx, rx) = mpsc::channel::<Msg>(256);
    tokio::spawn(run(client, eventloop, rx, prefix));
    Ok(tx)
}

async fn run(
    client: AsyncClient,
    mut eventloop: EventLoop,
    mut rx: mpsc::Receiver<Msg>,
    prefix: String,
) {
    let ha_status = format!("{prefix}/status");
    let mut latest: BTreeMap<String, Reading> = BTreeMap::new();
    let mut backoff = Duration::from_secs(1);
    loop {
        tokio::select! {
            event = eventloop.poll() => match event {
                Ok(Event::Incoming(Packet::ConnAck(_))) => {
                    tracing::info!("connected to MQTT broker");
                    backoff = Duration::from_secs(1);
                    let _ = client.try_publish(format!("{BASE}/status"), QoS::AtLeastOnce, true, "online");
                    let _ = client.try_subscribe(&ha_status, QoS::AtLeastOnce);
                    for (id, reading) in &latest {
                        announce(&client, &prefix, id);
                        publish_state(&client, id, reading);
                    }
                }
                Ok(Event::Incoming(Packet::Publish(p)))
                    if p.topic == ha_status && p.payload.as_ref() == b"online" =>
                {
                    tracing::info!("Home Assistant came online; re-sending discovery");
                    for (id, reading) in &latest {
                        announce(&client, &prefix, id);
                        publish_state(&client, id, reading);
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(error = %e, "MQTT connection failed; retrying in {backoff:?}");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(60));
                }
            },
            msg = rx.recv() => match msg {
                Some((id, reading)) => {
                    if !latest.contains_key(&id) {
                        announce(&client, &prefix, &id);
                    }
                    publish_state(&client, &id, &reading);
                    latest.insert(id, reading);
                }
                None => break,
            },
        }
    }
}

fn announce(client: &AsyncClient, prefix: &str, id: &str) {
    let (topic, payload) = discovery(prefix, id);
    if let Err(e) = client.try_publish(topic, QoS::AtLeastOnce, true, payload.to_string()) {
        tracing::debug!(error = %e, "MQTT discovery not queued");
    }
}

fn publish_state(client: &AsyncClient, id: &str, reading: &Reading) {
    let Ok(payload) = serde_json::to_vec(reading) else {
        return;
    };
    if let Err(e) = client.try_publish(state_topic(id), QoS::AtMostOnce, true, payload) {
        tracing::debug!(error = %e, "MQTT state not queued");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_broker_urls() {
        assert_eq!(
            parse_url("mqtt://ha.local").unwrap(),
            Broker {
                host: "ha.local".into(),
                port: 1883,
                login: None
            }
        );
        let b = parse_url("mqtt://user%40home:p%3Ass@10.0.0.5").unwrap();
        assert_eq!(b.port, 1883);
        assert_eq!(b.login, Some(("user@home".into(), "p:ss".into())));
        let err = parse_url("mqtts://10.0.0.5").unwrap_err().to_string();
        assert!(err.contains("not supported yet"), "{err}");
        assert_eq!(parse_url("mqtt://h:1999").unwrap().port, 1999);
        assert!(parse_url("http://h").is_err());
        assert!(parse_url("not a url").is_err());
    }

    #[test]
    fn discovery_describes_one_device_per_sensor() {
        let (topic, p) = discovery("homeassistant", "02AABBCCDDEE");
        assert_eq!(topic, "homeassistant/device/upaq_02aabbccddee/config");
        assert_eq!(p["device"]["identifiers"][0], "upaq_02aabbccddee");
        assert_eq!(p["device"]["connections"][0][1], "02:aa:bb:cc:dd:ee");
        assert_eq!(p["device"]["name"], "UP-AirQuality CCDDEE");
        assert_eq!(p["state_topic"], "up-airquality/02aabbccddee/state");
        assert_eq!(p["availability"][0]["topic"], "up-airquality/status");
        assert!(p["origin"]["name"].is_string());

        let c = p["components"].as_object().unwrap();
        assert_eq!(c.len(), 12);
        let co2 = &c["co2"];
        assert_eq!(co2["platform"], "sensor");
        assert_eq!(co2["device_class"], "carbon_dioxide");
        assert_eq!(co2["unit_of_measurement"], "ppm");
        assert_eq!(co2["state_class"], "measurement");
        assert_eq!(co2["value_template"], "{{ value_json.co2 }}");
        assert_eq!(co2["unique_id"], "upaq_02aabbccddee_co2");
        // Greek mu, as Home Assistant expects.
        assert_eq!(c["pm2p5"]["unit_of_measurement"], "\u{3bc}g/m\u{b3}");
        assert!(c["voc"].get("device_class").is_none());

        let ids: std::collections::BTreeSet<_> =
            c.values().map(|v| v["unique_id"].to_string()).collect();
        assert_eq!(ids.len(), 12, "unique_ids must be unique");
    }

    #[test]
    fn discovery_for_address_ids() {
        let (topic, p) = discovery("ha", "10.0.0.7");
        assert_eq!(topic, "ha/device/upaq_10_0_0_7/config");
        assert!(p["device"].get("connections").is_none());
        assert_eq!(p["state_topic"], "up-airquality/10_0_0_7/state");
    }
}
