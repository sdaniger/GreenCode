//! AWS Signature V4 (header-based signing), as Bedrock requires: the SHA-256 payload hash is always
//! signed (`x-amz-content-sha256`), unlike S3's UNSIGNED-PAYLOAD option.
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub struct Credentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}

/// What to attach to the request: `host`, `x-amz-date`, `x-amz-content-sha256`, `authorization`, and
/// `x-amz-security-token` for temporary credentials.
pub struct Signed {
    pub host: String,
    pub date: String,
    pub content_sha256: String,
    pub authorization: String,
    pub session_token: Option<String>,
}

/// `headers` are signed too (names case-insensitive); signer-owned and unsignable names are skipped.
/// `date` is `YYYYMMDDTHHMMSSZ`.
pub fn sign(
    method: &str,
    host: &str,
    path: &str,
    query: Option<&str>,
    headers: &BTreeMap<String, String>,
    body: &[u8],
    region: &str,
    service: &str,
    credentials: &Credentials,
    long_date: &str,
) -> Signed {
    let short_date = &long_date[..8];
    let payload_hash = hex(&Sha256::digest(body));

    let mut signed = BTreeMap::new();
    signed.insert("host".to_string(), host.to_string());
    signed.insert("x-amz-date".to_string(), long_date.to_string());
    signed.insert("x-amz-content-sha256".to_string(), payload_hash.clone());
    if let Some(token) = &credentials.session_token {
        signed.insert("x-amz-security-token".to_string(), token.clone());
    }
    for (name, value) in headers {
        let name = name.to_ascii_lowercase();
        if UNSIGNABLE.contains(&name.as_str()) || name.starts_with("proxy-") || name.starts_with("sec-") {
            continue;
        }
        signed.insert(name, value.trim().split_whitespace().collect::<Vec<_>>().join(" "));
    }

    let names: Vec<&str> = signed.keys().map(String::as_str).collect();
    let canonical_headers: String = names.iter().map(|n| format!("{n}:{}\n", signed[*n])).collect();
    let signed_headers = names.join(";");
    let canonical_request = [
        method.to_ascii_uppercase(),
        canonical_path(path),
        canonical_query(query),
        canonical_headers,
        signed_headers.clone(),
        payload_hash.clone(),
    ]
    .join("\n");

    let scope = format!("{short_date}/{region}/{service}/aws4_request");
    let string_to_sign = ["AWS4-HMAC-SHA256".to_string(), long_date.to_string(), scope.clone(), hex(&Sha256::digest(canonical_request.as_bytes()))].join("\n");
    let key = signing_key(&credentials.secret_access_key, short_date, region, service);
    let signature = hex(&hmac(&key, string_to_sign.as_bytes()));
    Signed {
        host: host.to_string(),
        date: long_date.to_string(),
        content_sha256: payload_hash,
        authorization: format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            credentials.access_key_id
        ),
        session_token: credentials.session_token.clone(),
    }
}

/// The `YYYYMMDDTHHMMSSZ` form of a unix-millisecond timestamp.
pub fn amz_date(unix_ms: u128) -> String {
    let secs = (unix_ms / 1000) as u64;
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (hour, minute, second) = (rem / 3600, rem % 3600 / 60, rem % 60);
    // Civil date from days since the epoch (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + (month <= 2) as i64;
    format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z")
}

/// Headers the SDK never signs.
const UNSIGNABLE: &[&str] = &[
    "authorization",
    "cache-control",
    "connection",
    "expect",
    "from",
    "keep-alive",
    "max-forwards",
    "pragma",
    "referer",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "user-agent",
    "x-amzn-trace-id",
];

/// Each path segment is RFC 3986 encoded; `/` itself stays literal. Model ids carry `:` and `.`.
fn canonical_path(path: &str) -> String {
    path.split('/').map(encode).collect::<Vec<_>>().join("/")
}

/// RFC 3986 percent-encoding of one path segment (model ids carry `:`).
pub fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// AWS sorts query pairs by their *encoded* name and value, then by value.
fn canonical_query(query: Option<&str>) -> String {
    let Some(query) = query.filter(|q| !q.is_empty()) else { return String::new() };
    let mut pairs: Vec<(String, String)> = query
        .split('&')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let (name, value) = part.split_once('=').unwrap_or((part, ""));
            (encode(&decode(name)), encode(&decode(value)))
        })
        .collect();
    pairs.sort();
    pairs.iter().map(|(name, value)| format!("{name}={value}")).collect::<Vec<_>>().join("&")
}

/// Percent-decodes a query component so `encode` can re-encode it canonically.
fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let hex = bytes.get(at + 1..at + 3).and_then(|pair| std::str::from_utf8(pair).ok()).and_then(|pair| u8::from_str_radix(pair, 16).ok());
        match (bytes[at], hex) {
            (b'%', Some(byte)) => {
                out.push(byte);
                at += 3;
            }
            (b'+', _) => {
                out.push(b' ');
                at += 1;
            }
            (byte, _) => {
                out.push(byte);
                at += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn signing_key(secret: &str, date: &str, region: &str, service: &str) -> [u8; 32] {
    let date_key = hmac(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let region_key = hmac(&date_key, region.as_bytes());
    let service_key = hmac(&region_key, service.as_bytes());
    hmac(&service_key, b"aws4_request")
}

/// HMAC-SHA256; `sha2` gives the digest but no keyed mode, so this is the two-block construction.
fn hmac(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut block = [0u8; 64];
    if key.len() > 64 {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(block.map(|b| b ^ 0x36));
    inner.update(data);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(block.map(|b| b ^ 0x5c));
    outer.update(inner);
    outer.finalize().into()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credentials() -> Credentials {
        Credentials {
            access_key_id: "AKIDEXAMPLE".into(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
            session_token: None,
        }
    }

    #[test]
    fn the_hand_rolled_hmac_matches_rfc_4231() {
        assert_eq!(
            hex(&hmac(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn matches_the_s3_get_object_documented_example() {
        // S3 docs, "Example: GET Object": Range header signed alongside the payload hash.
        // Its secret differs from the test-suite one by `/` vs `+`.
        let mut headers = BTreeMap::new();
        headers.insert("range".to_string(), "bytes=0-9".to_string());
        let signed = sign(
            "GET",
            "examplebucket.s3.amazonaws.com",
            "/test.txt",
            None,
            &headers,
            b"",
            "us-east-1",
            "s3",
            &Credentials {
                access_key_id: "AKIAIOSFODNN7EXAMPLE".into(),
                secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
                session_token: None,
            },
            "20130524T000000Z",
        );
        assert_eq!(
            signed.authorization,
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, \
             SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, \
             Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
    }

    #[test]
    fn canonical_forms_follow_the_spec() {
        assert_eq!(canonical_path("/model/us.anthropic.claude-opus-5-5/converse-stream"), "/model/us.anthropic.claude-opus-5-5/converse-stream");
        assert_eq!(canonical_path("/a b/c:d"), "/a%20b/c%3Ad");
        // Encoded order: `%7B` (0x25...) sorts before `x`, unlike its decoded form.
        assert_eq!(canonical_query(Some("x=1&%7B=2&b=3")), "%7B=2&b=3&x=1");
        assert_eq!(canonical_query(None), "");
        // Temporary credentials put the security token in the signature.
        let session = Credentials { session_token: Some("token".into()), ..credentials() };
        let signed = sign("POST", "h", "/", None, &BTreeMap::new(), b"{}", "us-east-1", "bedrock", &session, "20250101T000000Z");
        assert!(signed.authorization.contains("x-amz-security-token"));
        assert_eq!(signed.session_token.as_deref(), Some("token"));
    }

    #[test]
    fn amz_date_formats_utc_without_a_date_library() {
        assert_eq!(amz_date(0), "19700101T000000Z");
        assert_eq!(amz_date(1_435_708_800_000), "20150701T000000Z");
        assert_eq!(amz_date(1_753_540_800_000), "20250726T144000Z");
    }
}
