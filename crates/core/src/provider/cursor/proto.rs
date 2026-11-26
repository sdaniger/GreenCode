//! Hand-rolled protobuf and Connect framing for Cursor's agent RPCs.
//!
//! OMP (`providers/cursor/proto/agent.proto`, `connect-frame.ts`) uses generated
//! codecs; this workspace has no protobuf runtime, so only the messages cursor.rs
//! touches are encoded here, plus `google.protobuf.Value` for tool schemas and
//! MCP arguments.

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Map, Value};

/// The same cap Devin's Connect client uses; Cursor's frames are far smaller.
pub(crate) const MAX_FRAME: usize = 16 * 1024 * 1024;

#[derive(Default)]
pub(crate) struct Wr(pub Vec<u8>);

impl Wr {
    fn key(&mut self, no: u32, wire: u8) {
        put_varint(&mut self.0, u64::from(no) << 3 | u64::from(wire));
    }

    /// Varints are always written, zeros included: the messages here put an empty
    /// message where a oneof member is set, so presence must survive encoding.
    pub fn varint(&mut self, no: u32, v: u64) {
        self.key(no, 0);
        put_varint(&mut self.0, v);
    }

    pub fn bool(&mut self, no: u32, v: bool) {
        self.varint(no, u64::from(v));
    }

    pub fn double(&mut self, no: u32, v: f64) {
        self.key(no, 1);
        self.0.extend_from_slice(&v.to_le_bytes());
    }

    pub fn bytes(&mut self, no: u32, v: &[u8]) {
        self.key(no, 2);
        put_varint(&mut self.0, v.len() as u64);
        self.0.extend_from_slice(v);
    }

    pub fn str(&mut self, no: u32, v: &str) {
        self.bytes(no, v.as_bytes());
    }

    pub fn msg(&mut self, no: u32, v: &Wr) {
        self.bytes(no, &v.0);
    }
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn get_varint(b: &mut &[u8]) -> Option<u64> {
    let (mut out, mut shift) = (0u64, 0);
    loop {
        let (&byte, rest) = b.split_first()?;
        *b = rest;
        out |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(out);
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
}

pub(crate) enum Val<'a> {
    Varint(u64),
    Fixed64(f64),
    Bytes(&'a [u8]),
}

/// All length-delimited, varint and fixed64 fields; fixed32 fields are skipped.
pub(crate) fn fields(mut b: &[u8]) -> Result<Vec<(u32, Val<'_>)>> {
    let mut out = Vec::new();
    while !b.is_empty() {
        let key = get_varint(&mut b).ok_or_else(|| anyhow!("protobuf: truncated field key"))?;
        let no = (key >> 3) as u32;
        match (key & 7) as u8 {
            0 => {
                let v = get_varint(&mut b).ok_or_else(|| anyhow!("protobuf: truncated varint"))?;
                out.push((no, Val::Varint(v)));
            }
            1 => {
                if b.len() < 8 {
                    bail!("protobuf: truncated fixed64");
                }
                let bytes: [u8; 8] = b[..8].try_into().unwrap();
                b = &b[8..];
                out.push((no, Val::Fixed64(f64::from_le_bytes(bytes))));
            }
            2 => {
                let len = get_varint(&mut b).ok_or_else(|| anyhow!("protobuf: truncated length"))? as usize;
                if b.len() < len {
                    bail!("protobuf: truncated bytes field");
                }
                let (v, rest) = b.split_at(len);
                out.push((no, Val::Bytes(v)));
                b = rest;
            }
            5 => {
                if b.len() < 4 {
                    bail!("protobuf: truncated fixed32");
                }
                b = &b[4..];
            }
            other => bail!("protobuf: unsupported wire type {other}"),
        }
    }
    Ok(out)
}

pub(crate) fn vbytes<'a>(f: &[(u32, Val<'a>)], no: u32) -> Option<&'a [u8]> {
    f.iter().find_map(|(n, v)| match v {
        Val::Bytes(b) if *n == no => Some(*b),
        _ => None,
    })
}

pub(crate) fn vstr<'a>(f: &[(u32, Val<'a>)], no: u32) -> Option<&'a str> {
    vbytes(f, no).and_then(|b| std::str::from_utf8(b).ok())
}

pub(crate) fn vnum(f: &[(u32, Val<'_>)], no: u32) -> Option<u64> {
    f.iter().find_map(|(n, v)| match v {
        Val::Varint(x) if *n == no => Some(*x),
        _ => None,
    })
}

/// Encodes a JSON value as `google.protobuf.Value`.
pub(crate) fn json_value(v: &Value) -> Vec<u8> {
    let mut w = Wr::default();
    write_value(&mut w, v);
    w.0
}

fn write_value(w: &mut Wr, v: &Value) {
    match v {
        Value::Null => w.varint(1, 0),
        Value::Bool(b) => w.bool(4, *b),
        Value::Number(n) => w.double(2, n.as_f64().unwrap_or(0.0)),
        Value::String(s) => w.str(3, s),
        Value::Array(items) => {
            let mut list = Wr::default();
            for item in items {
                list.bytes(1, &json_value(item));
            }
            w.msg(6, &list);
        }
        Value::Object(map) => {
            let mut st = Wr::default();
            for (k, item) in map {
                let mut entry = Wr::default();
                entry.str(1, k);
                entry.bytes(2, &json_value(item));
                st.msg(1, &entry);
            }
            w.msg(5, &st);
        }
    }
}

/// Decodes `google.protobuf.Value` bytes.
pub(crate) fn json_from_value(bytes: &[u8]) -> Result<Value> {
    for (no, val) in fields(bytes)? {
        match (no, val) {
            (1, _) => return Ok(Value::Null),
            // Value has no integer type; whole numbers keep their integer form so a
            // tool schema or MCP argument round-trips unchanged.
            (2, Val::Fixed64(f)) if f.fract() == 0.0 && f.abs() < 9.007_199_254_740_992e15 => return Ok(json!(f as i64)),
            (2, Val::Fixed64(f)) => return Ok(json!(f)),
            (3, Val::Bytes(b)) => return Ok(json!(String::from_utf8_lossy(b).into_owned())),
            (4, Val::Varint(v)) => return Ok(json!(v != 0)),
            (5, Val::Bytes(b)) => {
                let mut map = Map::new();
                for (n, entry) in fields(b)? {
                    let Val::Bytes(entry) = entry else { continue };
                    if n != 1 {
                        continue;
                    }
                    let f = fields(entry)?;
                    let key = vstr(&f, 1).unwrap_or("");
                    if let Some(value) = vbytes(&f, 2) {
                        map.insert(key.into(), json_from_value(value)?);
                    }
                }
                return Ok(Value::Object(map));
            }
            (6, Val::Bytes(b)) => {
                let mut items = Vec::new();
                for (n, value) in fields(b)? {
                    if n != 1 {
                        continue;
                    }
                    if let Val::Bytes(value) = value {
                        items.push(json_from_value(value)?);
                    }
                }
                return Ok(Value::Array(items));
            }
            _ => {}
        }
    }
    Err(anyhow!("protobuf: Value has no known field"))
}

/// One Connect stream frame: 1 flag byte, u32 length, payload.
pub(crate) fn frame(flags: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + payload.len());
    out.push(flags);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// Incremental Connect frame decoder: a header or payload split across chunks is joined.
#[derive(Default)]
pub(crate) struct Frames {
    buf: Vec<u8>,
}

impl Frames {
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<(u8, Vec<u8>)>> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        let mut off = 0;
        while self.buf.len() - off >= 5 {
            let flags = self.buf[off];
            let len = u32::from_be_bytes([self.buf[off + 1], self.buf[off + 2], self.buf[off + 3], self.buf[off + 4]]) as usize;
            if len > MAX_FRAME {
                bail!("Cursor の応答フレームが大きすぎます（{len} バイト）");
            }
            if self.buf.len() - off < 5 + len {
                break;
            }
            out.push((flags, self.buf[off + 5..off + 5 + len].to_vec()));
            off += 5 + len;
        }
        self.buf.drain(..off);
        Ok(out)
    }
}

/// The end-of-stream trailer's error, as a user-facing error.
pub(crate) fn trailer_error(payload: &[u8]) -> Option<anyhow::Error> {
    let v: Value = serde_json::from_slice(payload).ok()?;
    let err = if v.get("error").is_some_and(Value::is_object) { &v["error"] } else { &v };
    let code = err["code"].as_str().unwrap_or("");
    let message = err["message"].as_str().unwrap_or("");
    if code.is_empty() && message.is_empty() {
        return None;
    }
    let mut text = String::from("Cursor がリクエストを拒否しました");
    if !code.is_empty() {
        text.push_str(&format!("（{code}）"));
    }
    if !message.is_empty() {
        text.push_str(&format!(": {message}"));
    }
    Some(anyhow!(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_decode_across_chunk_boundaries_and_end_stream_errors_surface() {
        let schema = json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]});
        let payload = json_value(&schema);
        assert_eq!(json_from_value(&payload).unwrap(), schema);
        let trailer = br#"{"error":{"code":"internal","message":"boom"}}"#;
        let wire = [frame(0, &payload), frame(0, b"second"), frame(0x02, trailer)].concat();

        let mut frames = Frames::default();
        // A header split across chunks, then a payload split across chunks.
        assert!(frames.push(&wire[..3]).unwrap().is_empty());
        let split = 4 + payload.len() / 2;
        assert!(frames.push(&wire[3..split]).unwrap().is_empty());
        let got = frames.push(&wire[split..]).unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!((got[0].0, got[0].1.as_slice()), (0, payload.as_slice()));
        assert_eq!((got[1].0, got[1].1.as_slice()), (0, b"second".as_slice()));
        assert_eq!(got[2].0, 0x02);
        let err = trailer_error(&got[2].1).unwrap();
        assert!(err.to_string().contains("boom"), "{err}");
        assert!(trailer_error(b"").is_none());
    }

    #[test]
    fn json_values_round_trip_through_google_protobuf_value() {
        let value = json!({"a": [1, "two", false, null], "b": {"c": 2.5}});
        assert_eq!(json_from_value(&json_value(&value)).unwrap(), value);
    }
}
