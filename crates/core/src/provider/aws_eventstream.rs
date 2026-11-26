//! `application/vnd.amazon.eventstream` framing, used by Bedrock's Converse and InvokeModel streams.
//!
//! Layout (big-endian): `total length u32`, `headers length u32`, `prelude CRC32`, headers, payload,
//! `message CRC32`. Headers are `[name length u8][name][value type u8][value]`; every value is a
//! string here, which is all Bedrock's `:message-type` / `:event-type` markers need.
use anyhow::{bail, Result};
use std::collections::BTreeMap;

/// aws-c-event-stream caps whole messages at 24 MiB; the bound keeps a corrupt length from allocating.
const MAX_MESSAGE: usize = 24 * 1024 * 1024;
const MAX_HEADERS: usize = 128 * 1024;
/// Prelude (12) + both CRCs (4 + 4) and no headers or payload.
const MIN_MESSAGE: usize = 16;

pub struct Frame {
    pub headers: BTreeMap<String, String>,
    pub payload: Vec<u8>,
}

/// CRC32 (IEEE, polynomial 0xEDB88320), the checksum both CRCs use.
pub fn crc32(bytes: &[u8]) -> u32 {
    const TABLE: [u32; 256] = {
        let mut table = [0u32; 256];
        let mut i = 0;
        while i < 256 {
            let mut c = i as u32;
            let mut k = 0;
            while k < 8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
                k += 1;
            }
            table[i] = c;
            i += 1;
        }
        table
    };
    let mut crc = !0u32;
    for &byte in bytes {
        crc = TABLE[((crc ^ byte as u32) & 0xff) as usize] ^ (crc >> 8);
    }
    !crc
}

/// Incremental decoder: a chunk may split a frame anywhere or carry several.
#[derive(Default)]
pub struct Decoder {
    buf: Vec<u8>,
}

impl Decoder {
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<Frame>> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(frame) = self.next()? {
            out.push(frame);
        }
        Ok(out)
    }

    /// True when the body ended inside a frame, which means it was truncated.
    pub fn pending(&self) -> bool {
        !self.buf.is_empty()
    }

    fn next(&mut self) -> Result<Option<Frame>> {
        if self.buf.len() < 12 {
            return Ok(None);
        }
        let u32_at = |at: usize| u32::from_be_bytes(self.buf[at..at + 4].try_into().unwrap());
        let (total, headers_len) = (u32_at(0) as usize, u32_at(4) as usize);
        if !(MIN_MESSAGE..=MAX_MESSAGE).contains(&total) {
            bail!("eventstream のフレーム長が不正です（{total}）");
        }
        if headers_len > MAX_HEADERS || headers_len > total - MIN_MESSAGE {
            bail!("eventstream のヘッダ長が不正です（{headers_len}）");
        }
        if crc32(&self.buf[..8]) != u32_at(8) {
            bail!("eventstream の prelude が壊れています");
        }
        if self.buf.len() < total {
            return Ok(None);
        }
        let frame: Vec<u8> = self.buf.drain(..total).collect();
        if crc32(&frame[..total - 4]) != u32::from_be_bytes(frame[total - 4..].try_into().unwrap()) {
            bail!("eventstream のメッセージが壊れています");
        }
        Ok(Some(Frame {
            headers: headers(&frame[12..12 + headers_len])?,
            payload: frame[12 + headers_len..total - 4].to_vec(),
        }))
    }
}

fn headers(buf: &[u8]) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    let mut at = 0;
    // Bounds-check every read: a short header block must not panic on a hostile stream.
    let take = |at: &mut usize, len: usize| -> Result<&[u8]> {
        let slice = buf.get(*at..*at + len).ok_or_else(|| anyhow::anyhow!("eventstream のヘッダが途中で切れています"))?;
        *at += len;
        Ok(slice)
    };
    while at < buf.len() {
        let name_len = take(&mut at, 1)?[0] as usize;
        let name = String::from_utf8_lossy(take(&mut at, name_len)?).into_owned();
        let kind = take(&mut at, 1)?[0];
        let number = |at: &mut usize, len: usize| -> Result<i64> {
            let bytes = take(at, len)?;
            Ok(match len {
                1 => bytes[0] as i8 as i64,
                2 => i16::from_be_bytes(bytes.try_into().unwrap()) as i64,
                4 => i32::from_be_bytes(bytes.try_into().unwrap()) as i64,
                _ => i64::from_be_bytes(bytes.try_into().unwrap()),
            })
        };
        let value = match kind {
            0 => "true".into(),
            1 => "false".into(),
            2..=5 => number(&mut at, 1 << (kind - 2))?.to_string(),
            // Bedrock sets no long/timestamp/uuid headers; render them generically rather than fail.
            6 => {
                let len = u16::from_be_bytes(take(&mut at, 2)?.try_into().unwrap()) as usize;
                base64::Engine::encode(&base64::engine::general_purpose::STANDARD, take(&mut at, len)?)
            }
            7 => {
                let len = u16::from_be_bytes(take(&mut at, 2)?.try_into().unwrap()) as usize;
                String::from_utf8_lossy(take(&mut at, len)?).into_owned()
            }
            8 => number(&mut at, 8)?.to_string(),
            9 => take(&mut at, 16)?.iter().map(|b| format!("{b:02x}")).collect(),
            other => bail!("eventstream のヘッダ型 {other} は未対応です"),
        };
        out.insert(name, value);
    }
    Ok(out)
}

/// A framed message with both CRCs computed, for tests and local fixtures.
#[cfg(test)]
pub(crate) fn encode(headers: &[(&str, &str)], payload: &[u8]) -> Vec<u8> {
    let mut block = Vec::new();
    for (name, value) in headers {
        block.push(name.len() as u8);
        block.extend_from_slice(name.as_bytes());
        block.push(7);
        block.extend_from_slice(&(value.len() as u16).to_be_bytes());
        block.extend_from_slice(value.as_bytes());
    }
    let total = 12 + block.len() + payload.len() + 4;
    let mut frame = Vec::with_capacity(total);
    frame.extend_from_slice(&(total as u32).to_be_bytes());
    frame.extend_from_slice(&(block.len() as u32).to_be_bytes());
    let prelude = crc32(&frame[..8]);
    frame.extend_from_slice(&prelude.to_be_bytes());
    frame.extend_from_slice(&block);
    frame.extend_from_slice(payload);
    let message = crc32(&frame);
    frame.extend_from_slice(&message.to_be_bytes());
    frame
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_the_published_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn frames_survive_arbitrary_chunk_boundaries() {
        let one = encode(&[(":event-type", "messageStart"), (":content-type", "application/json")], br#"{"role":"assistant"}"#);
        let two = encode(&[(":event-type", "contentBlockDelta")], b"{}");
        let mut all = one.clone();
        all.extend_from_slice(&two);

        // One byte at a time is the worst case; the payload must still come out whole.
        let mut decoder = Decoder::default();
        let mut frames = Vec::new();
        for byte in &all {
            frames.extend(decoder.push(std::slice::from_ref(byte)).unwrap());
        }
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].headers[":event-type"], "messageStart");
        assert_eq!(frames[0].payload, br#"{"role":"assistant"}"#);
        assert!(!frames[1].headers.contains_key(":content-type"));
        assert!(!decoder.pending());

        // A body that ends inside the second frame stays pending.
        let mut short = Decoder::default();
        assert_eq!(short.push(&all[..all.len() - 1]).unwrap().len(), 1);
        assert!(short.pending());
    }

    #[test]
    fn a_corrupt_frame_is_rejected_instead_of_misread() {
        let mut frame = encode(&[(":event-type", "messageStop")], b"{}");
        let last = frame.len() - 1;
        frame[last] ^= 0xff;
        assert!(Decoder::default().push(&frame).is_err());
    }
}
