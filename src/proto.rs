//! UCP4 "binme" framing used by UP-AirQuality <-> Protect console.
//!
//! A single logical message is two concatenated frames inside one WebSocket
//! binary message:
//!   1. a HEAD frame (frame_type = 1) carrying the envelope JSON
//!   2. a BODY frame (frame_type = 2) carrying the payload JSON
//!
//! Each frame starts with an 8-byte header:
//!   [0] frame_type    1 = head, 2 = body
//!   [1] payload_format 1 = JSON (the only one we emit)
//!   [2] deflated      0 = raw, 1 = zlib-compressed payload
//!   [3] reserved      0
//!   [4..8] length     u32, big-endian, of the (possibly compressed) payload
//!
//! This mirrors the UniFi Protect realtime packet format documented by
//! hjdhjd/unifi-protect, which the firmware strings confirm the sensor reuses.

use anyhow::{bail, Context, Result};
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use serde_json::Value;
use std::io::{Read, Write};

pub const FRAME_HEAD: u8 = 1;
pub const FRAME_BODY: u8 = 2;
pub const FMT_JSON: u8 = 1;

/// One decoded frame: its header fields plus the decompressed payload bytes.
#[derive(Debug, Clone)]
pub struct Frame {
    pub frame_type: u8,
    /// Retained for completeness/debugging; only JSON (1) is produced today.
    #[allow(dead_code)]
    pub payload_format: u8,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn json(&self) -> Result<Value> {
        serde_json::from_slice(&self.payload).context("frame payload is not valid JSON")
    }
}

/// A full logical message: envelope (head) + payload (body), already parsed.
#[derive(Debug, Clone)]
pub struct Message {
    pub head: Value,
    pub body: Value,
}

impl Message {
    pub fn action(&self) -> Option<&str> {
        self.head.get("action").and_then(Value::as_str)
    }
    pub fn msg_type(&self) -> Option<&str> {
        self.head.get("type").and_then(Value::as_str)
    }
    pub fn name(&self) -> Option<&str> {
        self.head.get("name").and_then(Value::as_str)
    }
    pub fn id(&self) -> Option<&str> {
        self.head.get("id").and_then(Value::as_str)
    }
}

/// Upper bound on the decoded size of all frames in one message. Real sensor
/// messages are a few hundred bytes; the cap stops a zlib bomb (~1000:1) from
/// exhausting memory.
pub const MAX_DECODED_BYTES: usize = 1024 * 1024;

/// Inflate at most `budget` bytes; error rather than allocate past it.
fn inflate(data: &[u8], budget: usize) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    ZlibDecoder::new(data)
        .take(budget as u64 + 1)
        .read_to_end(&mut out)
        .context("zlib inflate failed")?;
    if out.len() > budget {
        bail!("decoded message exceeds {MAX_DECODED_BYTES} bytes");
    }
    Ok(out)
}

fn deflate(data: &[u8]) -> Result<Vec<u8>> {
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
    enc.write_all(data)?;
    Ok(enc.finish()?)
}

/// Parse every frame in one WebSocket binary message. The decoded size of all
/// frames together is capped at [`MAX_DECODED_BYTES`].
pub fn decode_frames(mut buf: &[u8]) -> Result<Vec<Frame>> {
    let mut frames = Vec::new();
    let mut budget = MAX_DECODED_BYTES;
    while !buf.is_empty() {
        if buf.len() < 8 {
            bail!("truncated frame header ({} bytes left)", buf.len());
        }
        let frame_type = buf[0];
        let payload_format = buf[1];
        let deflated = buf[2];
        let len = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
        buf = &buf[8..];
        if buf.len() < len {
            bail!("truncated frame payload: need {}, have {}", len, buf.len());
        }
        let raw = &buf[..len];
        buf = &buf[len..];
        let payload = if deflated == 1 {
            inflate(raw, budget)?
        } else {
            raw.to_vec()
        };
        budget = budget
            .checked_sub(payload.len())
            .with_context(|| format!("decoded message exceeds {MAX_DECODED_BYTES} bytes"))?;
        frames.push(Frame {
            frame_type,
            payload_format,
            payload,
        });
    }
    Ok(frames)
}

/// Decode a WS binary message into head+body. Tolerant of a lone head frame.
pub fn decode_message(buf: &[u8]) -> Result<Message> {
    let frames = decode_frames(buf)?;
    let head = frames
        .iter()
        .find(|f| f.frame_type == FRAME_HEAD)
        .context("no head frame in message")?
        .json()?;
    let body = match frames.iter().find(|f| f.frame_type == FRAME_BODY) {
        Some(f) => f.json()?,
        None => Value::Null,
    };
    Ok(Message { head, body })
}

fn encode_frame(frame_type: u8, value: &Value, deflate_it: bool) -> Result<Vec<u8>> {
    let json = serde_json::to_vec(value)?;
    let (flag, payload) = if deflate_it {
        (1u8, deflate(&json)?)
    } else {
        (0u8, json)
    };
    let mut out = Vec::with_capacity(8 + payload.len());
    out.push(frame_type);
    out.push(FMT_JSON);
    out.push(flag);
    out.push(0);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(&payload);
    Ok(out)
}

/// Encode head+body into one WS binary message. We send uncompressed (the
/// deflated flag is honoured on receive either way).
pub fn encode_message(head: &Value, body: &Value) -> Result<Vec<u8>> {
    let mut out = encode_frame(FRAME_HEAD, head, false)?;
    out.extend(encode_frame(FRAME_BODY, body, false)?);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn roundtrip_plain() {
        let head = json!({"type": "event", "name": "readingEvent", "id": "abc"});
        let body = json!({"aqi": 42, "temperature": 21.5});
        let bytes = encode_message(&head, &body).unwrap();
        let msg = decode_message(&bytes).unwrap();
        assert_eq!(msg.name(), Some("readingEvent"));
        assert_eq!(msg.body["aqi"], 42);
    }

    #[test]
    fn decodes_deflated_frame() {
        // Build a deflated body frame by hand and confirm we inflate it.
        let body = json!({"co2": 800});
        let raw = serde_json::to_vec(&body).unwrap();
        let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
        enc.write_all(&raw).unwrap();
        let comp = enc.finish().unwrap();
        let mut frame = vec![FRAME_BODY, FMT_JSON, 1, 0];
        frame.extend_from_slice(&(comp.len() as u32).to_be_bytes());
        frame.extend_from_slice(&comp);
        let frames = decode_frames(&frame).unwrap();
        assert_eq!(frames[0].json().unwrap()["co2"], 800);
    }

    fn deflated_frame(frame_type: u8, raw: &[u8]) -> Vec<u8> {
        let mut enc = ZlibEncoder::new(Vec::new(), Compression::best());
        enc.write_all(raw).unwrap();
        let comp = enc.finish().unwrap();
        let mut frame = vec![frame_type, FMT_JSON, 1, 0];
        frame.extend_from_slice(&(comp.len() as u32).to_be_bytes());
        frame.extend_from_slice(&comp);
        frame
    }

    #[test]
    fn rejects_zlib_bomb() {
        // 8 MiB of zeros compresses to a few KiB; it must not be inflated.
        let frame = deflated_frame(FRAME_BODY, &vec![0u8; 8 * 1024 * 1024]);
        assert!(frame.len() < 64 * 1024);
        let err = decode_frames(&frame).unwrap_err();
        assert!(err.to_string().contains("exceeds"), "{err}");
    }

    #[test]
    fn caps_total_across_frames() {
        // Two frames that are each under the cap but together exceed it.
        let half = vec![b' '; MAX_DECODED_BYTES / 2 + 1];
        let mut msg = deflated_frame(FRAME_HEAD, &half);
        msg.extend(deflated_frame(FRAME_BODY, &half));
        assert!(decode_frames(&msg).is_err());
    }
}
