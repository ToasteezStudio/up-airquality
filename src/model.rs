//! Reading schema emitted by the sensor's `readingEvent`.
//!
//! Field names and units are from the firmware strings plus the console-side
//! schema in hjdhjd/unifi-protect (`airQuality`). The sensor chip is a
//! Sensirion SEN66. All fields are optional: the device may omit some while a
//! reading stabilises.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Reading {
    /// Air Quality Index (integer scale).
    pub aqi: Option<f64>,
    /// Vape-detection index, ~0..100.
    pub vape: Option<f64>,
    /// Total VOC, parts-per-billion-ish float.
    pub tvoc: Option<f64>,
    /// Particulate matter, µg/m³.
    pub pm1p0: Option<f64>,
    pub pm2p5: Option<f64>,
    pub pm4p0: Option<f64>,
    pub pm10p0: Option<f64>,
    /// Relative humidity, %RH.
    pub humidity: Option<f64>,
    /// Temperature, °C.
    pub temperature: Option<f64>,
    /// VOC index, 1..500.
    pub voc: Option<f64>,
    /// NOx index, 1..500.
    pub nox: Option<f64>,
    /// CO2, ppm.
    pub co2: Option<f64>,
}

impl Reading {
    /// A compact one-line summary for console output.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        let mut push = |label: &str, v: Option<f64>, unit: &str| {
            if let Some(v) = v {
                parts.push(format!("{label} {v:.1}{unit}"));
            }
        };
        push("AQI", self.aqi, "");
        push("T", self.temperature, "°C");
        push("RH", self.humidity, "%");
        push("CO2", self.co2, "ppm");
        push("PM2.5", self.pm2p5, "µg/m³");
        push("PM10", self.pm10p0, "µg/m³");
        push("VOC", self.voc, "");
        push("NOx", self.nox, "");
        push("vape", self.vape, "");
        if parts.is_empty() {
            "(no recognised fields)".into()
        } else {
            parts.join("  ")
        }
    }
}
