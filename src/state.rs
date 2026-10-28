//! Shared latest-reading state, updated by the console and read by the HTTP
//! dashboard / metrics endpoints.

use crate::model::Reading;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;

#[derive(Default)]
struct Inner {
    last: Option<Reading>,
    updated_ms: u64,
    count: u64,
    peer: Option<String>,
}

#[derive(Default)]
pub struct Metrics {
    inner: Mutex<Inner>,
    /// Optional sink to a Postgres/Timescale writer task.
    writer: Option<mpsc::Sender<(Reading, String)>>,
}

#[derive(Clone)]
pub struct Snapshot {
    pub last: Option<Reading>,
    pub updated_ms: u64,
    pub count: u64,
    pub peer: Option<String>,
}

impl Metrics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_writer(writer: mpsc::Sender<(Reading, String)>) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            writer: Some(writer),
        }
    }

    pub fn record(&self, reading: Reading, peer: &str) {
        if let Some(tx) = &self.writer {
            // Non-blocking: drop (with the writer's own warning) if the DB is
            // down and the buffer is full, rather than stalling the ws loop.
            let _ = tx.try_send((reading.clone(), peer.to_string()));
        }
        let mut g = self.inner.lock().unwrap();
        g.last = Some(reading);
        g.updated_ms = now_ms();
        g.count += 1;
        g.peer = Some(peer.to_string());
    }

    pub fn snapshot(&self) -> Snapshot {
        let g = self.inner.lock().unwrap();
        Snapshot {
            last: g.last.clone(),
            updated_ms: g.updated_ms,
            count: g.count,
            peer: g.peer.clone(),
        }
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
