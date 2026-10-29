//! Shared latest-reading state, one entry per sensor, updated by the console
//! and read by the HTTP dashboard / metrics endpoints.

use crate::model::Reading;
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;

/// Most sensors tracked at once. Ids come from the device, so without a
/// `--host` allowlist this bounds what a misbehaving client can make us keep.
pub const MAX_SENSORS: usize = 64;

/// Latest state for one sensor.
#[derive(Clone, Default)]
pub struct Sensor {
    pub last: Option<Reading>,
    pub updated_ms: u64,
    pub count: u64,
    pub peer: Option<String>,
}

#[derive(Default)]
pub struct Metrics {
    sensors: Mutex<BTreeMap<String, Sensor>>,
    /// Optional sink to a Postgres/Timescale writer task.
    writer: Option<mpsc::Sender<(Reading, String)>>,
    /// Optional sink to the MQTT / Home Assistant publisher.
    mqtt: Option<mpsc::Sender<crate::mqtt::Msg>>,
}

/// Point-in-time copy of every sensor, ordered by sensor id.
#[derive(Clone, Default)]
pub struct Snapshot {
    pub sensors: BTreeMap<String, Sensor>,
}

impl Snapshot {
    /// The most recently updated sensor (backs the single-sensor fields).
    pub fn latest(&self) -> Option<(&String, &Sensor)> {
        self.sensors.iter().max_by_key(|(_, s)| s.updated_ms)
    }
}

impl Metrics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_writer(mut self, writer: mpsc::Sender<(Reading, String)>) -> Self {
        self.writer = Some(writer);
        self
    }

    pub fn with_mqtt(mut self, mqtt: mpsc::Sender<crate::mqtt::Msg>) -> Self {
        self.mqtt = Some(mqtt);
        self
    }

    /// Record a reading from sensor `id`, which connected from `peer`.
    pub fn record(&self, id: &str, reading: Reading, peer: &str) {
        let mut g = self.sensors.lock().unwrap();
        if !g.contains_key(id) && g.len() >= MAX_SENSORS {
            tracing::warn!(
                id,
                peer,
                "sensor limit ({MAX_SENSORS}) reached; ignoring reading"
            );
            return;
        }
        if let Some(tx) = &self.writer {
            // Non-blocking: drop (with the writer's own warning) if the DB is
            // down and the buffer is full, rather than stalling the ws loop.
            let _ = tx.try_send((reading.clone(), peer.to_string()));
        }
        if let Some(tx) = &self.mqtt {
            // Same policy: the publisher re-sends the latest state on reconnect.
            let _ = tx.try_send((id.to_string(), reading.clone()));
        }
        let s = g.entry(id.to_string()).or_default();
        s.last = Some(reading);
        s.updated_ms = now_ms();
        s.count += 1;
        s.peer = Some(peer.to_string());
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            sensors: self.sensors.lock().unwrap().clone(),
        }
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading(co2: f64) -> Reading {
        Reading {
            co2: Some(co2),
            ..Default::default()
        }
    }

    #[test]
    fn keeps_sensors_separate() {
        let m = Metrics::new();
        m.record("AAAAAAAAAAAA", reading(500.0), "10.0.0.1");
        m.record("BBBBBBBBBBBB", reading(900.0), "10.0.0.2");
        m.record("AAAAAAAAAAAA", reading(510.0), "10.0.0.1");
        let snap = m.snapshot();
        assert_eq!(snap.sensors.len(), 2);
        let a = &snap.sensors["AAAAAAAAAAAA"];
        assert_eq!(a.count, 2);
        assert_eq!(a.last.as_ref().unwrap().co2, Some(510.0));
        assert_eq!(
            snap.sensors["BBBBBBBBBBBB"].peer.as_deref(),
            Some("10.0.0.2")
        );
    }

    #[test]
    fn caps_number_of_sensors() {
        let m = Metrics::new();
        for i in 0..MAX_SENSORS + 5 {
            m.record(&format!("{i:012X}"), reading(1.0), "10.0.0.1");
        }
        assert_eq!(m.snapshot().sensors.len(), MAX_SENSORS);
        // Known sensors keep updating at the cap.
        m.record(&format!("{:012X}", 0), reading(2.0), "10.0.0.1");
        assert_eq!(m.snapshot().sensors[&format!("{:012X}", 0)].count, 2);
    }
}
