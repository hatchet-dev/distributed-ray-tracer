//! Reading a shape script from a Hatchet durable stream, without a gRPC client.
//!
//! The app Worker publishes each script once, as the only message on the topic
//! `script-<id>` (the id is the SHA-256 of the text, in hex). An adapter reads it back with the
//! engine's `v1.V1Streams/Subscribe` call over the Connect protocol, which is plain HTTP: POST
//! one enveloped protobuf request, then read enveloped protobuf messages from the response body
//! until the first entry arrives. This module builds the request and decodes the response; the
//! adapter does the I/O.

use base64::Engine;

/// The Connect path of the engine's subscribe call, appended to [`engine_url`].
pub const SUBSCRIBE_PATH: &str = "/v1.V1Streams/Subscribe";
/// Headers the subscribe request needs (plus `Authorization: Bearer <token>`).
pub const SUBSCRIBE_HEADERS: [(&str, &str); 2] =
    [("Content-Type", "application/connect+proto"), ("Connect-Protocol-Version", "1")];

/// The durable stream topic a script is published to.
pub fn topic(id: &str) -> String {
    format!("script-{id}")
}

/// The engine's base URL, from the `grpc_broadcast_address` claim of a tenant API token (the
/// address the SDKs connect to), e.g. `https://engine.example.com:443`.
pub fn engine_url(token: &str) -> Option<String> {
    let claims = token.split('.').nth(1)?;
    let json = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(claims.trim_end_matches('=')).ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&json).ok()?;
    let address = claims.get("grpc_broadcast_address")?.as_str()?;
    Some(format!("https://{address}"))
}

/// The enveloped `SubscribeStreamRequest { topic }` body: the topic from its oldest message.
pub fn subscribe_body(topic: &str) -> Vec<u8> {
    let mut message = vec![0x12]; // field 2 (topic), length-delimited
    put_varint(&mut message, topic.len() as u64);
    message.extend_from_slice(topic.as_bytes());

    let mut body = vec![0]; // envelope flags: an uncompressed message
    body.extend_from_slice(&(message.len() as u32).to_be_bytes());
    body.extend(message);
    body
}

/// What the response bytes read so far hold.
#[derive(Debug, PartialEq)]
pub enum Read {
    /// The first message's payload: the script.
    Payload(Vec<u8>),
    /// Nothing yet; read more.
    More,
}

/// Decodes the Connect response body read so far (`buf`, from its start) until the first
/// stream entry. `Err` when the engine ended the stream (with its error, if any) before one.
pub fn first_payload(buf: &[u8]) -> Result<Read, String> {
    let mut at = 0;
    while buf.len() >= at + 5 {
        let flags = buf[at];
        let len = u32::from_be_bytes([buf[at + 1], buf[at + 2], buf[at + 3], buf[at + 4]]) as usize;
        let Some(frame) = buf.get(at + 5..at + 5 + len) else { return Ok(Read::More) };
        at += 5 + len;
        if flags & 0x02 != 0 {
            // End of stream: JSON, with an `error` if the call failed.
            let end: serde_json::Value = serde_json::from_slice(frame).unwrap_or_default();
            return Err(match end.pointer("/error/message").and_then(|m| m.as_str()) {
                Some(message) => format!("subscribe failed: {message}"),
                None => "the topic has no message".into(),
            });
        }
        if flags & 0x01 != 0 {
            return Err("the engine sent a compressed message".into());
        }
        // StreamMessage { repeated StreamEntry entries = 1; bool hangup = 2; ... }
        for (field, value) in fields(frame)? {
            match (field, value) {
                (1, Value::Bytes(entry)) => return entry_payload(entry).map(Read::Payload),
                (2, Value::Varint(1)) => return Err("the topic has no message".into()),
                _ => {}
            }
        }
    }
    Ok(Read::More)
}

/// StreamEntry { bytes payload = 1; ...; string payload_ref = 4 }
fn entry_payload(entry: &[u8]) -> Result<Vec<u8>, String> {
    let mut payload = Vec::new();
    for (field, value) in fields(entry)? {
        match (field, value) {
            (1, Value::Bytes(b)) => payload = b.to_vec(),
            (4, Value::Bytes(r)) if !r.is_empty() => {
                return Err("the script was published by reference, so it is too large to read inline".into())
            }
            _ => {}
        }
    }
    Ok(payload)
}

enum Value<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    Fixed,
}

/// The (field number, value) pairs of a protobuf message.
fn fields(mut buf: &[u8]) -> Result<Vec<(u64, Value<'_>)>, String> {
    let mut out = Vec::new();
    while !buf.is_empty() {
        let key = varint(&mut buf)?;
        let value = match key & 7 {
            0 => Value::Varint(varint(&mut buf)?),
            1 | 5 => {
                let n = if key & 7 == 1 { 8 } else { 4 };
                buf = buf.get(n..).ok_or("truncated protobuf")?;
                Value::Fixed
            }
            2 => {
                let n = varint(&mut buf)? as usize;
                let (bytes, rest) = (buf.get(..n).ok_or("truncated protobuf")?, &buf[n..]);
                buf = rest;
                Value::Bytes(bytes)
            }
            wire => return Err(format!("unsupported protobuf wire type {wire}")),
        };
        out.push((key >> 3, value));
    }
    Ok(out)
}

fn varint(buf: &mut &[u8]) -> Result<u64, String> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let (&byte, rest) = buf.split_first().ok_or("truncated protobuf")?;
        *buf = rest;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err("protobuf varint too long".into())
}

fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(flags: u8, message: &[u8]) -> Vec<u8> {
        let mut out = vec![flags];
        out.extend_from_slice(&(message.len() as u32).to_be_bytes());
        out.extend_from_slice(message);
        out
    }

    fn length_delimited(field: u8, bytes: &[u8]) -> Vec<u8> {
        let mut out = vec![(field << 3) | 2];
        put_varint(&mut out, bytes.len() as u64);
        out.extend_from_slice(bytes);
        out
    }

    /// A StreamMessage with one entry carrying `payload`, plus a cursor and timestamp to skip.
    fn message(payload: &[u8]) -> Vec<u8> {
        let mut entry = length_delimited(1, payload);
        entry.extend(length_delimited(2, b"cursor-1"));
        entry.extend(length_delimited(3, &[0x08, 0x96, 0x01])); // created_at { seconds: 150 }
        let mut msg = length_delimited(1, &entry);
        msg.extend(length_delimited(3, b"cursor-1"));
        msg
    }

    #[test]
    fn encodes_the_subscribe_request() {
        assert_eq!(subscribe_body("t"), vec![0, 0, 0, 0, 3, 0x12, 1, b't']);
        let long = "x".repeat(200); // a two-byte length varint
        assert_eq!(&subscribe_body(&long)[5..8], &[0x12, 0xc8, 0x01]);
    }

    #[test]
    fn reads_the_first_payload_across_chunks() {
        let script = "sphere([0, 0, 1], 1)".repeat(1000).into_bytes();
        let body = envelope(0, &message(&script));
        for cut in [0, 3, 5, 100, body.len() - 1] {
            assert_eq!(first_payload(&body[..cut]), Ok(Read::More), "cut at {cut}");
        }
        assert_eq!(first_payload(&body), Ok(Read::Payload(script)));
    }

    #[test]
    fn reports_the_end_of_stream_error() {
        let end = envelope(2, br#"{"error":{"code":"unauthenticated","message":"bad token"}}"#);
        assert_eq!(first_payload(&end), Err("subscribe failed: bad token".into()));
        assert!(first_payload(&envelope(2, b"{}")).unwrap_err().contains("no message"));
        let hangup = envelope(0, &[0x10, 0x01]);
        assert!(first_payload(&hangup).unwrap_err().contains("no message"));
    }

    #[test]
    fn engine_url_comes_from_the_token() {
        let claims = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"sub":"t","grpc_broadcast_address":"engine.example.com:443"}"#);
        assert_eq!(engine_url(&format!("h.{claims}.s")).as_deref(), Some("https://engine.example.com:443"));
        assert_eq!(engine_url("not a token"), None);
    }
}
