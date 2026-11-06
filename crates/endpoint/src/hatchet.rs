//! Hatchet serverless endpoint contract: request verification and wire types.
//! See https://docs.hatchet.run/v1/serverless/http

use hmac::{Hmac, Mac};
use serde::{Deserialize, Deserializer};
use serde_json::{json, Value};
use sha2::Sha256;

/// Reject requests whose timestamp is further than this from our clock.
const MAX_CLOCK_SKEW_SECS: i64 = 5 * 60;

/// Fields common to the healthcheck and trigger bodies.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Envelope {
    pub endpoint_id: String,
    /// Hatchet encodes requests with protojson, which writes int64 as a JSON string.
    #[serde(deserialize_with = "int_or_string")]
    pub timestamp_unix_seconds: i64,
    #[serde(default)]
    pub action: Option<Action>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Action {
    pub action_id: String,
    /// JSON string: `{"input": {...}, "parents": {...}, ...}`
    pub action_payload: String,
}

#[derive(Deserialize)]
pub struct ActionPayload<T> {
    pub input: T,
}

/// Accept `123` or `"123"`.
fn int_or_string<'de, D: Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum IntOrString {
        Int(i64),
        Str(String),
    }
    match IntOrString::deserialize(d)? {
        IntOrString::Int(n) => Ok(n),
        IntOrString::Str(s) => s.parse().map_err(serde::de::Error::custom),
    }
}

/// Why a request was rejected before any work was done.
pub enum Rejection {
    BadSignature,
    Malformed(String),
    Stale,
    WrongEndpoint,
}

/// Verify the HMAC signature (constant time) over the raw body, then the timestamp and
/// (optionally) the endpoint id, in the order the contract specifies.
pub fn verify(
    raw_body: &[u8],
    signature_hex: &str,
    secret: &str,
    now_unix_secs: i64,
    expected_endpoint_id: Option<&str>,
) -> Result<Envelope, Rejection> {
    let sig = hex::decode(signature_hex.trim()).map_err(|_| Rejection::BadSignature)?;
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).map_err(|_| Rejection::BadSignature)?;
    mac.update(raw_body);
    mac.verify_slice(&sig).map_err(|_| Rejection::BadSignature)?;

    let env: Envelope = serde_json::from_slice(raw_body).map_err(|e| Rejection::Malformed(e.to_string()))?;
    if (env.timestamp_unix_seconds - now_unix_secs).abs() > MAX_CLOCK_SKEW_SECS {
        return Err(Rejection::Stale);
    }
    if expected_endpoint_id.is_some_and(|id| id != env.endpoint_id) {
        return Err(Rejection::WrongEndpoint);
    }
    Ok(env)
}

/// Body of an error response. `retry` overrides Hatchet's status-code based retry decision.
pub fn error_body(message: impl Into<String>, retry: bool) -> Value {
    json!({ "error": message.into(), "retry": retry })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "0123456789abcdef0123456789abcdef";

    fn sign(body: &[u8], secret: &str) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        hex::encode(mac.finalize().into_bytes())
    }

    #[test]
    fn accepts_valid_request_with_protojson_timestamp() {
        let body = br#"{"endpointId":"ep1","timestampUnixSeconds":"1000"}"#;
        let env = verify(body, &sign(body, SECRET), SECRET, 1100, Some("ep1")).ok().unwrap();
        assert_eq!(env.timestamp_unix_seconds, 1000);
    }

    #[test]
    fn rejects_bad_signature_stale_and_wrong_endpoint() {
        let body = br#"{"endpointId":"ep1","timestampUnixSeconds":1000}"#;
        let sig = sign(body, SECRET);
        assert!(matches!(verify(body, "00", SECRET, 1000, None), Err(Rejection::BadSignature)));
        assert!(matches!(verify(body, &sig, SECRET, 1301, None), Err(Rejection::Stale)));
        assert!(matches!(verify(body, &sig, SECRET, 1000, Some("ep2")), Err(Rejection::WrongEndpoint)));
    }
}
