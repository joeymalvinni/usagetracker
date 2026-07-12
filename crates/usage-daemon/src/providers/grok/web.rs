//! Minimal gRPC-web client and protobuf projection for grok.com's billing RPC.

use chrono::{DateTime, Utc};
use reqwest::{header::HeaderMap, StatusCode};
use std::time::Duration;

use crate::providers::{
    read_response_body, retry_after_deadline, ProviderError, ProviderErrorKind,
};

use super::billing::BillingData;

const ENDPOINT: &str = "https://grok.com/grok_api_v2.GrokBuildBilling/GetGrokCreditsConfig";
const MAX_PROTO_FIELDS: usize = 50_000;
const RETRY_DELAYS: [Duration; 2] = [Duration::from_millis(150), Duration::from_millis(400)];

struct AttemptError {
    error: ProviderError,
    retryable: bool,
}

impl AttemptError {
    fn permanent(error: ProviderError) -> Self {
        Self {
            error,
            retryable: false,
        }
    }

    fn retryable(error: ProviderError) -> Self {
        Self {
            error,
            retryable: true,
        }
    }
}

pub(super) async fn fetch(
    client: &reqwest::Client,
    bearer: Option<&str>,
    cookie: Option<&str>,
) -> Result<BillingData, ProviderError> {
    fetch_with_endpoint(client, ENDPOINT, bearer, cookie).await
}

async fn fetch_with_endpoint(
    client: &reqwest::Client,
    endpoint: &str,
    bearer: Option<&str>,
    cookie: Option<&str>,
) -> Result<BillingData, ProviderError> {
    if bearer.is_none() && cookie.is_none() {
        return Err(ProviderError::new(
            ProviderErrorKind::CredentialsMissing,
            "Grok web billing requires grok login or a signed-in grok.com browser session",
        ));
    }
    let mut retry_delays = RETRY_DELAYS.into_iter();
    loop {
        match fetch_once(client, endpoint, bearer, cookie).await {
            Ok(data) => return Ok(data),
            Err(error) if error.retryable => {
                let Some(delay) = retry_delays.next() else {
                    return Err(error.error);
                };
                tokio::time::sleep(delay).await;
            }
            Err(error) => return Err(error.error),
        }
    }
}

async fn fetch_once(
    client: &reqwest::Client,
    endpoint: &str,
    bearer: Option<&str>,
    cookie: Option<&str>,
) -> Result<BillingData, AttemptError> {
    let mut request = client
        .post(endpoint)
        .header("Origin", "https://grok.com")
        .header("Referer", "https://grok.com/?_s=usage")
        .header("Accept", "*/*")
        .header("Content-Type", "application/grpc-web+proto")
        .header("x-grpc-web", "1")
        .header("x-user-agent", "connect-es/2.1.1")
        .body(vec![0_u8; 5]);
    if let Some(bearer) = bearer {
        request = request.bearer_auth(bearer);
    }
    if let Some(cookie) = cookie {
        request = request.header("Cookie", cookie);
    }
    let response = request.send().await.map_err(|err| {
        AttemptError::retryable(ProviderError::new(
            ProviderErrorKind::Network,
            format!("Grok web billing request failed: {err}"),
        ))
    })?;
    let status = response.status();
    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        return Err(AttemptError::permanent(ProviderError::new(
            ProviderErrorKind::Unauthorized,
            "Grok web billing rejected the current session; run `grok login` or sign in to grok.com in Chrome",
        )));
    }
    if status == StatusCode::TOO_MANY_REQUESTS {
        return Err(AttemptError::permanent(
            ProviderError::new(
                ProviderErrorKind::RateLimited,
                "Grok web billing was rate limited",
            )
            .with_retry_at(retry_after_deadline(response.headers())),
        ));
    }
    if !status.is_success() {
        let error = ProviderError::new(
            ProviderErrorKind::ProviderUnavailable,
            format!("Grok web billing returned HTTP {}", status.as_u16()),
        );
        return Err(if retryable_http_status(status) {
            AttemptError::retryable(error)
        } else {
            AttemptError::permanent(error)
        });
    }
    validate_grpc_headers(response.headers())?;
    let body = read_response_body(response, "Grok web billing response")
        .await
        .map_err(|error| {
            if error.kind() == ProviderErrorKind::Network {
                AttemptError::retryable(error)
            } else {
                AttemptError::permanent(error)
            }
        })?;
    validate_grpc_trailers(&body)?;
    parse_response(&body, Utc::now()).map_err(AttemptError::permanent)
}

fn retryable_http_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::REQUEST_TIMEOUT
            | StatusCode::BAD_GATEWAY
            | StatusCode::SERVICE_UNAVAILABLE
            | StatusCode::GATEWAY_TIMEOUT
    )
}

fn validate_grpc_headers(headers: &HeaderMap) -> Result<(), AttemptError> {
    let status = headers
        .get("grpc-status")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(0);
    let message = headers
        .get("grpc-message")
        .and_then(|value| value.to_str().ok())
        .map(decode_grpc_message)
        .unwrap_or_default();
    grpc_status(status, &message)
}

fn validate_grpc_trailers(data: &[u8]) -> Result<(), AttemptError> {
    for (flags, payload) in frames(data) {
        if flags & 0x80 == 0 {
            continue;
        }
        let text = String::from_utf8_lossy(payload);
        let mut status = 0;
        let mut message = String::new();
        for line in text.lines() {
            if let Some(value) = line.strip_prefix("grpc-status:") {
                status = value.trim().parse().unwrap_or(0);
            }
            if let Some(value) = line.strip_prefix("grpc-message:") {
                message = decode_grpc_message(value.trim());
            }
        }
        grpc_status(status, &message)?;
    }
    Ok(())
}

fn grpc_status(status: u16, message: &str) -> Result<(), AttemptError> {
    if status == 0 {
        return Ok(());
    }
    let lower = message.to_ascii_lowercase();
    let auth_rejected = status == 16
        || (status == 7
            && [
                "bad-credentials",
                "unauthenticated",
                "expired",
                "oauth2",
                "validation fail",
            ]
            .iter()
            .any(|needle| lower.contains(needle)));
    let retryable =
        status == 4 || (status == 1 && (lower.contains("timeout") || lower.contains("deadline")));
    let kind = if auth_rejected {
        ProviderErrorKind::Unauthorized
    } else if status == 8 || lower.contains("rate limit") {
        ProviderErrorKind::RateLimited
    } else {
        ProviderErrorKind::ProviderUnavailable
    };
    let guidance = if auth_rejected {
        "; run `grok login` or sign in to grok.com in Chrome"
    } else {
        ""
    };
    let error = ProviderError::new(
        kind,
        format!("Grok web billing RPC failed with status {status}: {message}{guidance}"),
    );
    Err(if retryable {
        AttemptError::retryable(error)
    } else {
        AttemptError::permanent(error)
    })
}

fn decode_grpc_message(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (hex(bytes[index + 1]), hex(bytes[index + 2])) {
                decoded.push(high << 4 | low);
                index += 3;
                continue;
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn hex(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

pub(super) fn parse_response(
    data: &[u8],
    now: DateTime<Utc>,
) -> Result<BillingData, ProviderError> {
    let framed = frames(data);
    let payloads = framed
        .iter()
        .filter(|(flags, _)| flags & 0x80 == 0)
        .map(|(_, payload)| *payload)
        .collect::<Vec<_>>();
    let payloads = if payloads.is_empty() && looks_like_protobuf(data) {
        vec![data]
    } else {
        payloads
    };
    if payloads.is_empty() {
        return Err(ProviderError::new(
            ProviderErrorKind::Parse,
            "Grok web billing returned no protobuf data frame",
        ));
    }
    let mut scan = ProtoScan::default();
    for payload in payloads {
        scan_payload(payload, &[], 0, &mut scan);
    }
    let percent = scan
        .fixed32
        .iter()
        .filter(|field| {
            field.path.last() == Some(&1)
                && field.value.is_finite()
                && (0.0..=100.0).contains(&field.value)
        })
        .min_by_key(|field| (field.path.len(), field.order))
        .map(|field| f64::from(field.value));
    let mut timestamps = scan
        .varints
        .iter()
        .filter_map(|field| {
            (1_700_000_000..=2_100_000_000)
                .contains(&field.value)
                .then(|| DateTime::from_timestamp(field.value as i64, 0))
                .flatten()
                .map(|date| (field.path.clone(), date))
        })
        .collect::<Vec<_>>();
    timestamps.sort_by_key(|(_, date)| *date);
    let resets_at = timestamps
        .iter()
        .filter(|(_, date)| *date > now)
        .find(|(path, _)| path.as_slice() == [1, 5, 1])
        .or_else(|| timestamps.iter().find(|(_, date)| *date > now))
        .map(|(_, date)| *date);
    let period_start = timestamps
        .iter()
        .rev()
        .find(|(_, date)| *date <= now)
        .map(|(_, date)| *date);
    let has_usage_period = scan
        .varints
        .iter()
        .any(|field| field.path.as_slice() == [1, 5, 1]);
    let used_percent = percent
        .or_else(|| (resets_at.is_some() && has_usage_period).then_some(0.0))
        .ok_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::Parse,
                "could not locate Grok usage percentage in billing protobuf",
            )
        })?;
    Ok(BillingData {
        used_percent,
        period_start,
        resets_at,
        used_usd: None,
        limit_usd: None,
        on_demand_used_usd: None,
        on_demand_limit_usd: None,
    })
}

fn frames(data: &[u8]) -> Vec<(u8, &[u8])> {
    let mut frames = Vec::new();
    let mut index = 0;
    while index < data.len() {
        if index + 5 > data.len() {
            return Vec::new();
        }
        let flags = data[index];
        let length = u32::from_be_bytes(data[index + 1..index + 5].try_into().unwrap()) as usize;
        let start = index + 5;
        let Some(end) = start.checked_add(length).filter(|end| *end <= data.len()) else {
            return Vec::new();
        };
        frames.push((flags, &data[start..end]));
        index = end;
    }
    frames
}

fn looks_like_protobuf(data: &[u8]) -> bool {
    data.first()
        .is_some_and(|byte| byte >> 3 > 0 && matches!(byte & 7, 0 | 1 | 2 | 5))
}

#[derive(Default)]
struct ProtoScan {
    fixed32: Vec<Fixed32>,
    varints: Vec<Varint>,
    order: usize,
    fields_seen: usize,
}
struct Fixed32 {
    path: Vec<u64>,
    value: f32,
    order: usize,
}
struct Varint {
    path: Vec<u64>,
    value: u64,
}

fn scan_payload(data: &[u8], path: &[u64], depth: usize, scan: &mut ProtoScan) {
    let mut index = 0;
    while index < data.len() {
        if scan.fields_seen >= MAX_PROTO_FIELDS {
            return;
        }
        let start = index;
        let Some(key) = read_varint(data, &mut index).filter(|key| *key != 0) else {
            index = start + 1;
            continue;
        };
        scan.fields_seen += 1;
        let field_path = path
            .iter()
            .copied()
            .chain(std::iter::once(key >> 3))
            .collect::<Vec<_>>();
        match key & 7 {
            0 => {
                if let Some(value) = read_varint(data, &mut index) {
                    scan.varints.push(Varint {
                        path: field_path,
                        value,
                    });
                } else {
                    index = start + 1;
                }
            }
            1 => {
                if index + 8 <= data.len() {
                    index += 8;
                } else {
                    break;
                }
            }
            2 => {
                let Some(length) =
                    read_varint(data, &mut index).and_then(|value| usize::try_from(value).ok())
                else {
                    index = start + 1;
                    continue;
                };
                let Some(end) = index.checked_add(length).filter(|end| *end <= data.len()) else {
                    index = start + 1;
                    continue;
                };
                if depth < 4 {
                    scan_payload(&data[index..end], &field_path, depth + 1, scan);
                }
                index = end;
            }
            5 => {
                if index + 4 <= data.len() {
                    let value = f32::from_bits(u32::from_le_bytes(
                        data[index..index + 4].try_into().unwrap(),
                    ));
                    scan.fixed32.push(Fixed32 {
                        path: field_path,
                        value,
                        order: scan.order,
                    });
                    scan.order += 1;
                    index += 4;
                } else {
                    break;
                }
            }
            _ => index = start + 1,
        }
    }
}

fn read_varint(data: &[u8], index: &mut usize) -> Option<u64> {
    let mut value = 0_u64;
    let mut shift = 0;
    while *index < data.len() && shift < 64 {
        let byte = data[*index];
        *index += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
        shift += 7;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn varint(mut value: u64) -> Vec<u8> {
        let mut bytes = Vec::new();
        loop {
            let mut byte = (value & 0x7f) as u8;
            value >>= 7;
            if value != 0 {
                byte |= 0x80;
            }
            bytes.push(byte);
            if value == 0 {
                return bytes;
            }
        }
    }

    fn field(field: u8, payload: &[u8]) -> Vec<u8> {
        let mut encoded = vec![field << 3 | 2];
        encoded.extend(varint(payload.len() as u64));
        encoded.extend_from_slice(payload);
        encoded
    }

    #[test]
    fn parses_framed_percent_and_prefers_future_reset() {
        let mut payload = vec![0x0d];
        payload.extend_from_slice(&42.5_f32.to_bits().to_le_bytes());
        payload.push(0x10);
        payload.extend(varint(1_800_000_000));
        payload.push(0x18);
        payload.extend(varint(1_802_592_000));
        let mut framed = vec![0, 0, 0, 0, payload.len() as u8];
        framed.extend(payload);
        let data =
            parse_response(&framed, DateTime::from_timestamp(1_800_001_000, 0).unwrap()).unwrap();
        assert_eq!(data.used_percent, 42.5);
        assert_eq!(data.resets_at.unwrap().timestamp(), 1_802_592_000);
    }

    #[test]
    fn ignores_grpc_trailer_frames() {
        let trailer = b"grpc-status: 0\r\n";
        let mut framed = vec![0x80, 0, 0, 0, trailer.len() as u8];
        framed.extend(trailer);
        assert!(frames(&framed).iter().all(|(flags, _)| flags & 0x80 != 0));
    }

    #[test]
    fn parses_raw_nested_protobuf_and_preferred_reset_path() {
        let mut reset = vec![0x08];
        reset.extend(varint(1_802_592_000));
        let period = field(5, &reset);
        let mut billing = vec![0x0d];
        billing.extend_from_slice(&17.25_f32.to_bits().to_le_bytes());
        billing.extend(period);
        let payload = field(1, &billing);

        let data = parse_response(
            &payload,
            DateTime::from_timestamp(1_800_000_000, 0).unwrap(),
        )
        .unwrap();
        assert_eq!(data.used_percent, 17.25);
        assert_eq!(data.resets_at.unwrap().timestamp(), 1_802_592_000);
    }

    #[test]
    fn omitted_proto3_percent_is_zero_only_with_a_billing_period() {
        let mut reset = vec![0x08];
        reset.extend(varint(1_802_592_000));
        let payload = field(1, &field(5, &reset));
        let now = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        assert_eq!(parse_response(&payload, now).unwrap().used_percent, 0.0);

        let mut unrelated = vec![0x10];
        unrelated.extend(varint(1_802_592_000));
        assert_eq!(
            parse_response(&unrelated, now).unwrap_err().kind(),
            ProviderErrorKind::Parse
        );
    }

    #[test]
    fn classifies_retryable_and_auth_grpc_statuses() {
        let deadline = grpc_status(4, "deadline exceeded").unwrap_err();
        assert!(deadline.retryable);
        assert_eq!(
            deadline.error.kind(),
            ProviderErrorKind::ProviderUnavailable
        );

        let auth = grpc_status(7, "OAuth2%20validation%20failed").unwrap_err();
        assert!(!auth.retryable);
        assert_eq!(auth.error.kind(), ProviderErrorKind::Unauthorized);
    }

    #[test]
    fn decodes_percent_encoded_grpc_messages() {
        assert_eq!(
            decode_grpc_message("OAuth2%20token%20expired"),
            "OAuth2 token expired"
        );
    }

    #[tokio::test]
    async fn retries_transient_http_failures_with_a_bounded_budget() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for attempt in 0..3 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0_u8; 4096];
                let _ = socket.read(&mut request).await.unwrap();
                if attempt < 2 {
                    socket
                        .write_all(
                            b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await
                        .unwrap();
                    continue;
                }
                let mut payload = vec![0x0d];
                payload.extend_from_slice(&31.5_f32.to_bits().to_le_bytes());
                let mut body = vec![0_u8; 5];
                body[1..5].copy_from_slice(&(payload.len() as u32).to_be_bytes());
                body.extend(payload);
                let headers = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/grpc-web+proto\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                socket.write_all(headers.as_bytes()).await.unwrap();
                socket.write_all(&body).await.unwrap();
            }
            3
        });

        let client = reqwest::Client::new();
        let result = fetch_with_endpoint(
            &client,
            &format!("http://{address}/billing"),
            None,
            Some("sso=test"),
        )
        .await
        .unwrap();
        assert_eq!(result.used_percent, 31.5);
        assert_eq!(server.await.unwrap(), 3);
    }
}
