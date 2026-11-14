use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub(super) fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(DIGITS[(byte >> 4) as usize] as char);
        result.push(DIGITS[(byte & 15) as usize] as char);
    }
    result
}
pub(super) fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    const DIGITS: &[u8] = b"0123456789ABCDEF";
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(DIGITS[(b >> 4) as usize] as char);
            out.push(DIGITS[(b & 15) as usize] as char);
        }
    }
    out
}
fn decode(value: &str) -> Result<String, String> {
    let mut out = Vec::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let digits = bytes
                .get(i + 1..i + 3)
                .ok_or("Invalid URL percent escape")?;
            let digits = std::str::from_utf8(digits).map_err(|e| e.to_string())?;
            out.push(u8::from_str_radix(digits, 16).map_err(|e| e.to_string())?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|e| e.to_string())
}
fn canonical_query(query: Option<&str>) -> Result<String, String> {
    let mut pairs = Vec::new();
    for part in query
        .unwrap_or_default()
        .split('&')
        .filter(|p| !p.is_empty())
    {
        let (key, value) = part.split_once('=').unwrap_or((part, ""));
        pairs.push((encode(&decode(key)?), encode(&decode(value)?)));
    }
    pairs.sort();
    Ok(pairs
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&"))
}
fn hmac(key: &[u8], value: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts arbitrary key lengths");
    mac.update(value);
    mac.finalize().into_bytes().to_vec()
}
fn signable(name: &str) -> bool {
    !matches!(
        name,
        "authorization"
            | "cache-control"
            | "connection"
            | "expect"
            | "from"
            | "keep-alive"
            | "max-forwards"
            | "pragma"
            | "referer"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "user-agent"
            | "x-amzn-trace-id"
    ) && !name.starts_with("proxy-")
        && !name.starts_with("sec-")
}
pub(super) fn sign(
    url: &reqwest::Url,
    body: &[u8],
    region: &str,
    access: &str,
    secret: &str,
    token: Option<&str>,
    headers: &mut BTreeMap<String, String>,
    date: time::OffsetDateTime,
) -> Result<(), String> {
    let long_date = format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        date.year(),
        date.month() as u8,
        date.day(),
        date.hour(),
        date.minute(),
        date.second()
    );
    let short_date = &long_date[..8];
    let hash = hex(&Sha256::digest(body));
    let mut host = url
        .host_str()
        .ok_or("Bedrock endpoint has no host")?
        .to_owned();
    if let Some(port) = url.port() {
        host.push_str(&format!(":{port}"));
    }
    headers.insert("host".into(), host);
    headers.insert("x-amz-date".into(), long_date.clone());
    headers.insert("x-amz-content-sha256".into(), hash.clone());
    if let Some(token) = token {
        headers.insert("x-amz-security-token".into(), token.into());
    }
    let signed: BTreeMap<_, _> = headers
        .iter()
        .filter(|(k, _)| signable(k))
        .map(|(k, v)| {
            (
                k.as_str(),
                v.split_whitespace().collect::<Vec<_>>().join(" "),
            )
        })
        .collect();
    let names = signed.keys().copied().collect::<Vec<_>>().join(";");
    let canonical_headers: String = signed.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let path = url
        .path()
        .split('/')
        .map(encode)
        .collect::<Vec<_>>()
        .join("/");
    let canonical = format!(
        "POST\n{path}\n{}\n{canonical_headers}\n{names}\n{hash}",
        canonical_query(url.query())?
    );
    let scope = format!("{short_date}/{region}/bedrock/aws4_request");
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{long_date}\n{scope}\n{}",
        hex(&Sha256::digest(canonical.as_bytes()))
    );
    let date_key = hmac(format!("AWS4{secret}").as_bytes(), short_date.as_bytes());
    let region_key = hmac(&date_key, region.as_bytes());
    let service_key = hmac(&region_key, b"bedrock");
    let signing_key = hmac(&service_key, b"aws4_request");
    let signature = hex(&hmac(&signing_key, to_sign.as_bytes()));
    headers.insert("authorization".into(), format!("AWS4-HMAC-SHA256 Credential={access}/{scope}, SignedHeaders={names}, Signature={signature}"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn query_encodes_before_sorting_and_preserves_plus() {
        assert_eq!(
            canonical_query(Some("x=2&%7B=1&a=+&a=%20")).unwrap(),
            "%7B=1&a=%20&a=%2B&x=2"
        );
    }
    #[test]
    fn signs_session_credentials_and_custom_headers() {
        let url = reqwest::Url::parse(
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/a%3Ab/converse-stream?z=2&a=1",
        )
        .unwrap();
        let mut headers = BTreeMap::from([
            ("content-type".into(), "application/json".into()),
            ("x-custom".into(), "  a  b  ".into()),
        ]);
        sign(
            &url,
            b"{}",
            "us-east-1",
            "AKID",
            "secret",
            Some("token"),
            &mut headers,
            time::OffsetDateTime::UNIX_EPOCH,
        )
        .unwrap();
        assert_eq!(headers["x-amz-date"], "19700101T000000Z");
        assert_eq!(headers["x-amz-security-token"], "token");
        assert!(headers["authorization"]
            .contains("Credential=AKID/19700101/us-east-1/bedrock/aws4_request"));
        assert!(headers["authorization"].contains("SignedHeaders=content-type;host;x-amz-content-sha256;x-amz-date;x-amz-security-token;x-custom"));
    }
}
