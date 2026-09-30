//! Private modern Streamable HTTP transport with bounded JSON and SSE replies.
//! All requests use HTTPS, fixed DNS answers, no proxies, and no redirects.

// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2024-2026 axpnet: AI-assisted (see AI-TRANSPARENCY.md)

use std::collections::HashSet;
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use base64::Engine;
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use reqwest::{Client, StatusCode};
use serde_json::{Map, Value};
use tokio::net::lookup_host;
use tokio_util::sync::CancellationToken;
use url::{Host, Url};

use crate::mcp_client_http_config::{parse_public_https, McpHttpServerConfig};
use crate::mcp_client_protocol::{self as protocol, Era};

pub(crate) const MAX_REPLY_BYTES: usize = 64 * 1024;
const MAX_SCHEMA_HEADERS: usize = 24;
const MAX_HEADER_VALUE_BYTES: usize = 4096;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const SAFE_INTEGER: i64 = 9_007_199_254_740_991;

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct AuthChallenge {
    pub metadata_url: Option<String>,
    pub scopes: Vec<String>,
}

impl fmt::Debug for AuthChallenge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthChallenge")
            .field("metadata_url_present", &self.metadata_url.is_some())
            .field("scope_count", &self.scopes.len())
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HttpError {
    InvalidRequest,
    InvalidEndpoint,
    UnsafeAddress,
    Dns,
    Connect,
    Redirect,
    Unauthorized(AuthChallenge),
    Forbidden(AuthChallenge),
    UnsupportedVersion,
    ResponseTooLarge,
    InvalidResponse,
    Timeout,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HeaderAnnotation {
    name: HeaderName,
    path: Vec<String>,
    primitive: &'static str,
}

fn header_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
}

fn validate_unreachable(value: &Value) -> Result<(), HttpError> {
    match value {
        Value::Object(object) => {
            if object.contains_key("x-mcp-header") {
                return Err(HttpError::InvalidRequest);
            }
            for child in object.values() {
                validate_unreachable(child)?;
            }
        }
        Value::Array(array) => {
            for child in array {
                validate_unreachable(child)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn collect_annotations(
    schema: &Value,
    path: &mut Vec<String>,
    reachable: bool,
    names: &mut HashSet<String>,
    output: &mut Vec<HeaderAnnotation>,
) -> Result<(), HttpError> {
    let object = schema.as_object().ok_or(HttpError::InvalidRequest)?;
    if let Some(raw) = object.get("x-mcp-header") {
        let suffix = raw.as_str().ok_or(HttpError::InvalidRequest)?;
        let primitive = object
            .get("type")
            .and_then(Value::as_str)
            .ok_or(HttpError::InvalidRequest)?;
        if !reachable
            || path.is_empty()
            || !header_token(suffix)
            || !matches!(primitive, "string" | "integer" | "boolean")
            || output.len() >= MAX_SCHEMA_HEADERS
            || !names.insert(suffix.to_ascii_lowercase())
        {
            return Err(HttpError::InvalidRequest);
        }
        let name = HeaderName::from_bytes(format!("Mcp-Param-{suffix}").as_bytes())
            .map_err(|_| HttpError::InvalidRequest)?;
        output.push(HeaderAnnotation {
            name,
            path: path.clone(),
            primitive: match primitive {
                "string" => "string",
                "integer" => "integer",
                _ => "boolean",
            },
        });
    }
    for (key, value) in object {
        if key == "x-mcp-header" {
            continue;
        }
        if key == "properties"
            && reachable
            && object.get("type").and_then(Value::as_str) == Some("object")
        {
            let properties = value.as_object().ok_or(HttpError::InvalidRequest)?;
            for (property, child) in properties {
                path.push(property.clone());
                collect_annotations(child, path, true, names, output)?;
                path.pop();
            }
        } else {
            validate_unreachable(value)?;
        }
    }
    Ok(())
}

fn annotations(schema: &Value) -> Result<Vec<HeaderAnnotation>, HttpError> {
    if schema.get("type").and_then(Value::as_str) != Some("object") {
        return Err(HttpError::InvalidRequest);
    }
    let mut output = Vec::new();
    collect_annotations(
        schema,
        &mut Vec::new(),
        true,
        &mut HashSet::new(),
        &mut output,
    )?;
    Ok(output)
}

fn mirrored_value(value: &str) -> Result<HeaderValue, HttpError> {
    if value.len() > MAX_HEADER_VALUE_BYTES {
        return Err(HttpError::InvalidRequest);
    }
    let plain = value
        .bytes()
        .all(|byte| (0x20..=0x7e).contains(&byte) || byte == b'\t')
        && value.trim() == value
        && !(value.starts_with("=?base64?") && value.ends_with("?="));
    let encoded = if plain {
        value.to_owned()
    } else {
        format!(
            "=?base64?{}?=",
            base64::engine::general_purpose::STANDARD.encode(value.as_bytes())
        )
    };
    HeaderValue::from_str(&encoded).map_err(|_| HttpError::InvalidRequest)
}

fn argument_at<'a>(arguments: &'a Value, path: &[String]) -> Option<&'a Value> {
    path.iter().try_fold(arguments, |value, key| value.get(key))
}

fn parameter_headers(schema: &Value, arguments: &Value) -> Result<HeaderMap, HttpError> {
    let mut headers = HeaderMap::new();
    for annotation in annotations(schema)? {
        let Some(value) = argument_at(arguments, &annotation.path) else {
            continue;
        };
        if value.is_null() {
            continue;
        }
        let rendered = match annotation.primitive {
            "string" => value.as_str().ok_or(HttpError::InvalidRequest)?.to_owned(),
            "boolean" => value
                .as_bool()
                .ok_or(HttpError::InvalidRequest)?
                .to_string(),
            "integer" => {
                let number = value.as_i64().ok_or(HttpError::InvalidRequest)?;
                if !(-SAFE_INTEGER..=SAFE_INTEGER).contains(&number) {
                    return Err(HttpError::InvalidRequest);
                }
                number.to_string()
            }
            _ => return Err(HttpError::InvalidRequest),
        };
        headers.insert(annotation.name, mirrored_value(&rendered)?);
    }
    Ok(headers)
}

fn request_headers(
    method: &str,
    params: &Map<String, Value>,
    schema: Option<&Value>,
) -> Result<HeaderMap, HttpError> {
    if method.is_empty()
        || method.len() > 128
        || !method.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
    {
        return Err(HttpError::InvalidRequest);
    }
    let mut headers = HeaderMap::new();
    headers.insert(
        ACCEPT,
        HeaderValue::from_static("application/json, text/event-stream"),
    );
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(
        "mcp-protocol-version",
        HeaderValue::from_static(protocol::MODERN_VERSION),
    );
    headers.insert(
        "mcp-method",
        HeaderValue::from_str(method).map_err(|_| HttpError::InvalidRequest)?,
    );
    if matches!(method, "tools/call" | "resources/read" | "prompts/get") {
        let field = if method == "resources/read" {
            "uri"
        } else {
            "name"
        };
        let name = params
            .get(field)
            .and_then(Value::as_str)
            .ok_or(HttpError::InvalidRequest)?;
        headers.insert("mcp-name", mirrored_value(name)?);
    }
    if method == "tools/call" {
        let schema = schema.ok_or(HttpError::InvalidRequest)?;
        let arguments = params.get("arguments").ok_or(HttpError::InvalidRequest)?;
        if !arguments.is_object() {
            return Err(HttpError::InvalidRequest);
        }
        headers.extend(parameter_headers(schema, arguments)?);
    }
    Ok(headers)
}

fn public_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !(a == 0
                || a == 10
                || a == 127
                || a >= 224
                || (a == 100 && (64..=127).contains(&b))
                || (a == 169 && b == 254)
                || (a == 172 && (16..=31).contains(&b))
                || (a == 192 && (b == 0 || (b == 168) || (b == 88 && c == 99)))
                || (a == 198 && (b == 18 || b == 19 || (b == 51 && c == 100)))
                || (a == 203 && b == 0 && c == 113))
        }
        // IPv6 remains fail-closed until a complete special-use range policy
        // and platform-specific pinning tests are available.
        IpAddr::V6(ip) => {
            if let Some(mapped) = ip.to_ipv4_mapped() {
                return public_ip(IpAddr::V4(mapped));
            }
            let octets = ip.octets();
            (octets[0] & 0xe0) == 0x20
                && !(octets[0] == 0x20
                    && octets[1] == 0x01
                    && ((octets[2] == 0x0d && octets[3] == 0xb8)
                        || (octets[2] == 0x00 && octets[3] == 0x00)))
                && !(octets[0] == 0x20 && octets[1] == 0x02)
        }
    }
}

pub(crate) async fn pinned_client(
    url: &Url,
    cancel: &CancellationToken,
) -> Result<Client, HttpError> {
    let Host::Domain(host) = url.host().ok_or(HttpError::InvalidEndpoint)? else {
        return Err(HttpError::InvalidEndpoint);
    };
    let port = url
        .port_or_known_default()
        .ok_or(HttpError::InvalidEndpoint)?;
    let addresses = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(HttpError::Cancelled),
        result = tokio::time::timeout(Duration::from_secs(5), lookup_host((host, port))) => {
            result.map_err(|_| HttpError::Timeout)?
                .map_err(|_| HttpError::Dns)?
                .collect::<Vec<SocketAddr>>()
        }
    };
    if addresses.is_empty() || addresses.iter().any(|address| !public_ip(address.ip())) {
        return Err(HttpError::UnsafeAddress);
    }
    Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(REQUEST_TIMEOUT)
        .resolve_to_addrs(host, &addresses)
        .build()
        .map_err(|_| HttpError::Connect)
}

pub(crate) async fn bounded_body(
    response: reqwest::Response,
    cancel: &CancellationToken,
) -> Result<Vec<u8>, HttpError> {
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    loop {
        let chunk = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(HttpError::Cancelled),
            chunk = stream.next() => chunk,
        };
        let Some(chunk) = chunk else { return Ok(bytes) };
        let chunk = chunk.map_err(|_| HttpError::Connect)?;
        if bytes.len().saturating_add(chunk.len()) > MAX_REPLY_BYTES {
            return Err(HttpError::ResponseTooLarge);
        }
        bytes.extend_from_slice(&chunk);
    }
}

async fn bounded_sse(
    response: reqwest::Response,
    expected_id: u64,
    cancel: &CancellationToken,
) -> Result<Value, HttpError> {
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    loop {
        let chunk = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(HttpError::Cancelled),
            chunk = stream.next() => chunk,
        };
        let Some(chunk) = chunk else {
            return parse_sse(&bytes, expected_id);
        };
        let chunk = chunk.map_err(|_| HttpError::Connect)?;
        if bytes.len().saturating_add(chunk.len()) > MAX_REPLY_BYTES {
            return Err(HttpError::ResponseTooLarge);
        }
        bytes.extend_from_slice(&chunk);
        let lf_end = bytes
            .windows(2)
            .enumerate()
            .filter(|(_, window)| *window == b"\n\n")
            .map(|(index, _)| index + 2)
            .last();
        let crlf_end = bytes
            .windows(4)
            .enumerate()
            .filter(|(_, window)| *window == b"\r\n\r\n")
            .map(|(index, _)| index + 4)
            .last();
        let end = lf_end.into_iter().chain(crlf_end).max();
        if let Some(end) = end {
            if let Ok(result) = parse_sse(&bytes[..end], expected_id) {
                return Ok(result);
            }
        }
    }
}

fn parse_sse(bytes: &[u8], expected_id: u64) -> Result<Value, HttpError> {
    let input = std::str::from_utf8(bytes).map_err(|_| HttpError::InvalidResponse)?;
    let mut data = String::new();
    let mut result = None;
    for line in input.lines().chain(std::iter::once("")) {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            if data.is_empty() {
                continue;
            }
            let message: Value = serde_json::from_str(data.trim_end_matches('\n'))
                .map_err(|_| HttpError::InvalidResponse)?;
            data.clear();
            if message.get("method").is_some() {
                if message.get("id").is_some()
                    || message.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
                    || !message
                        .get("method")
                        .and_then(Value::as_str)
                        .is_some_and(|method| method.starts_with("notifications/"))
                {
                    return Err(HttpError::InvalidResponse);
                }
                continue;
            }
            if result.is_some() {
                return Err(HttpError::InvalidResponse);
            }
            result = Some(
                protocol::accept_result(&message, expected_id, Era::Modern)
                    .map_err(|_| HttpError::InvalidResponse)?
                    .clone(),
            );
        } else if let Some(value) = line.strip_prefix("data:") {
            data.push_str(value.strip_prefix(' ').unwrap_or(value));
            data.push('\n');
        } else if !line.starts_with(':')
            && !line.starts_with("event:")
            && !line.starts_with("id:")
            && !line.starts_with("retry:")
        {
            return Err(HttpError::InvalidResponse);
        }
    }
    result.ok_or(HttpError::InvalidResponse)
}

fn parse_challenge(value: Option<&HeaderValue>) -> AuthChallenge {
    let mut parsed = AuthChallenge {
        metadata_url: None,
        scopes: Vec::new(),
    };
    let Some(raw) = value.and_then(|value| value.to_str().ok()) else {
        return parsed;
    };
    if raw.len() > 4096
        || raw.len() < 7
        || !raw
            .get(..6)
            .is_some_and(|scheme| scheme.eq_ignore_ascii_case("Bearer"))
        || raw.as_bytes()[6] != b' '
    {
        return parsed;
    }
    for part in raw[7..].split(',') {
        let Some((key, value)) = part.trim().split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches('"');
        if value.chars().any(char::is_control) || value.len() > 2048 {
            continue;
        }
        match key.trim() {
            "resource_metadata" => {
                if parse_public_https(value, 2048).is_ok() {
                    parsed.metadata_url = Some(value.to_owned());
                }
            }
            "scope" => {
                parsed.scopes = value
                    .split_whitespace()
                    .filter(|scope| scope.len() <= 128)
                    .take(32)
                    .map(str::to_owned)
                    .collect();
            }
            _ => {}
        }
    }
    parsed
}

fn parse_reply(content_type: &str, bytes: &[u8], expected_id: u64) -> Result<Value, HttpError> {
    let media_type = content_type.split(';').next().unwrap_or("").trim();
    match media_type {
        "application/json" => {
            let message: Value =
                serde_json::from_slice(bytes).map_err(|_| HttpError::InvalidResponse)?;
            protocol::accept_result(&message, expected_id, Era::Modern)
                .map_err(|_| HttpError::InvalidResponse)
                .cloned()
        }
        "text/event-stream" => parse_sse(bytes, expected_id),
        _ => Err(HttpError::InvalidResponse),
    }
}

fn authorization_header(token: &str) -> Result<HeaderValue, HttpError> {
    if token.is_empty() || token.len() > MAX_HEADER_VALUE_BYTES {
        return Err(HttpError::InvalidRequest);
    }
    let mut value =
        HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| HttpError::InvalidRequest)?;
    value.set_sensitive(true);
    Ok(value)
}

pub(crate) async fn request(
    config: &McpHttpServerConfig,
    method: &str,
    params: Map<String, Value>,
    schema: Option<&Value>,
    token: Option<&str>,
    id: u64,
    cancel: &CancellationToken,
) -> Result<Value, HttpError> {
    config.validate().map_err(|_| HttpError::InvalidEndpoint)?;
    if !config.enabled {
        return Err(HttpError::InvalidEndpoint);
    }
    let url = parse_public_https(&config.endpoint, 2048).map_err(|_| HttpError::InvalidEndpoint)?;
    let mut headers = request_headers(method, &params, schema)?;
    if let Some(token) = token {
        headers.insert(AUTHORIZATION, authorization_header(token)?);
    }
    let body = protocol::request(
        Era::Modern,
        id,
        method,
        params,
        "AeroFTP",
        env!("CARGO_PKG_VERSION"),
    )
    .map_err(|_| HttpError::InvalidRequest)?;
    let body = serde_json::to_vec(&body).map_err(|_| HttpError::InvalidRequest)?;
    if body.len() > MAX_REPLY_BYTES {
        return Err(HttpError::InvalidRequest);
    }
    let client = pinned_client(&url, cancel).await?;
    send_prepared(&client, url, headers, body, id, cancel).await
}

async fn send_prepared(
    client: &Client,
    url: Url,
    headers: HeaderMap,
    body: Vec<u8>,
    id: u64,
    cancel: &CancellationToken,
) -> Result<Value, HttpError> {
    let response = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(HttpError::Cancelled),
        result = client.post(url).headers(headers).body(body).send() => result.map_err(|_| HttpError::Connect)?,
    };
    if response.status().is_redirection() {
        return Err(HttpError::Redirect);
    }
    if response.status() == StatusCode::UNAUTHORIZED {
        return Err(HttpError::Unauthorized(parse_challenge(
            response.headers().get("www-authenticate"),
        )));
    }
    if response.status() == StatusCode::FORBIDDEN {
        return Err(HttpError::Forbidden(parse_challenge(
            response.headers().get("www-authenticate"),
        )));
    }
    if response.status() == StatusCode::BAD_REQUEST {
        let body = bounded_body(response, cancel).await?;
        let error: Value = serde_json::from_slice(&body).map_err(|_| HttpError::InvalidResponse)?;
        if error.pointer("/error/code").and_then(Value::as_i64) == Some(-32022) {
            return Err(HttpError::UnsupportedVersion);
        }
        return Err(HttpError::InvalidResponse);
    }
    if !response.status().is_success() {
        return Err(HttpError::InvalidResponse);
    }
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .ok_or(HttpError::InvalidResponse)?
        .to_owned();
    if content_type
        .split(';')
        .next()
        .is_some_and(|media| media.trim() == "text/event-stream")
    {
        bounded_sse(response, id, cancel).await
    } else {
        let bytes = bounded_body(response, cancel).await?;
        parse_reply(&content_type, &bytes, id)
    }
}

/// Test-only local wire adapter. Production always constructs a DNS-pinned
/// public HTTPS client inside request; this helper is absent from app builds.
#[cfg(test)]
pub(crate) async fn fixture_exchange(
    url: Url,
    method: &str,
    params: Map<String, Value>,
    schema: Option<&Value>,
    id: u64,
    cancel: &CancellationToken,
) -> Result<Value, HttpError> {
    let headers = request_headers(method, &params, schema)?;
    let body = protocol::request(
        Era::Modern,
        id,
        method,
        params,
        "AeroFTP",
        env!("CARGO_PKG_VERSION"),
    )
    .map_err(|_| HttpError::InvalidRequest)?;
    let client = Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| HttpError::Connect)?;
    send_prepared(
        &client,
        url,
        headers,
        serde_json::to_vec(&body).map_err(|_| HttpError::InvalidRequest)?,
        id,
        cancel,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn bearer_header_is_sensitive_and_debug_redacted() {
        let value = authorization_header("fixture-bearer-secret").unwrap();
        assert!(value.is_sensitive());
        assert_eq!(value.to_str().unwrap(), "Bearer fixture-bearer-secret");
        assert!(!format!("{value:?}").contains("fixture-bearer-secret"));
        let mut map = HeaderMap::new();
        map.insert(AUTHORIZATION, value);
        assert!(!format!("{map:?}").contains("fixture-bearer-secret"));
        assert_eq!(
            authorization_header("bad\nvalue"),
            Err(HttpError::InvalidRequest)
        );
    }

    // Local test-only wire peer for response handling. Production still
    // constructs its own HTTPS client after DNS and address validation.
    async fn wire_fixture(
        response: Vec<u8>,
        delay: Duration,
    ) -> (Url, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let read = socket.read(&mut chunk).await.unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
                if let Some(split) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                    let header = String::from_utf8_lossy(&request[..split]);
                    let length = header
                        .lines()
                        .find_map(|line| {
                            line.split_once(':')
                                .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                                .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if request.len() >= split + 4 + length {
                        break;
                    }
                }
                assert!(request.len() <= MAX_REPLY_BYTES + 4096);
            }
            tokio::time::sleep(delay).await;
            let _ = socket.write_all(&response).await;
            String::from_utf8(request).unwrap()
        });
        (Url::parse(&format!("http://{address}/mcp")).unwrap(), task)
    }

    fn wire_reply(status: &str, content_type: &str, extra: &str, body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n{body}",
            body.len()
        ).into_bytes()
    }

    fn test_client() -> Client {
        Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap()
    }

    async fn fixture_request(response: Vec<u8>) -> (Result<Value, HttpError>, String) {
        let (url, peer) = wire_fixture(response, Duration::ZERO).await;
        let params = Map::from_iter([
            ("name".into(), json!("probe")),
            ("arguments".into(), json!({"region":"west"})),
        ]);
        let schema = json!({"type":"object","properties":{"region":{"type":"string","x-mcp-header":"Region"}}});
        let headers = request_headers("tools/call", &params, Some(&schema)).unwrap();
        let body = serde_json::to_vec(
            &protocol::request(Era::Modern, 7, "tools/call", params, "AeroFTP", "4.2").unwrap(),
        )
        .unwrap();
        let result = send_prepared(
            &test_client(),
            url,
            headers,
            body,
            7,
            &CancellationToken::new(),
        )
        .await;
        (result, peer.await.unwrap())
    }

    #[tokio::test]
    async fn wire_fixture_checks_json_sse_headers_and_correlated_reply() {
        let json = r#"{"jsonrpc":"2.0","id":7,"result":{"resultType":"complete","content":[]}}"#;
        let (result, request) =
            fixture_request(wire_reply("200 OK", "application/json", "", json)).await;
        assert_eq!(result.unwrap()["content"], json!([]));
        let request = request.to_ascii_lowercase();
        assert!(request.contains("mcp-method: tools/call\r\n"));
        assert!(request.contains("mcp-name: probe\r\n"));
        assert!(request.contains("mcp-param-region: west\r\n"));
        assert!(request.contains("mcp-protocol-version: 2026-07-28\r\n"));
        assert!(request.contains("io.modelcontextprotocol/protocolversion"));

        let sse = "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\ndata: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"resultType\":\"complete\",\"content\":[]}}\n\n";
        let (result, _) = fixture_request(wire_reply("200 OK", "text/event-stream", "", sse)).await;
        assert_eq!(result.unwrap()["content"], json!([]));
        let wrong = r#"{"jsonrpc":"2.0","id":8,"result":{"resultType":"complete"}}"#;
        let (result, _) =
            fixture_request(wire_reply("200 OK", "application/json", "", wrong)).await;
        assert_eq!(result, Err(HttpError::InvalidResponse));
    }

    #[tokio::test]
    async fn wire_fixture_rejects_redirect_large_body_and_handles_401_scope() {
        let (result, _) = fixture_request(wire_reply(
            "401 Unauthorized",
            "application/json",
            "WWW-Authenticate: Bearer scope=\"files:read files:write\"\r\n",
            "",
        ))
        .await;
        assert!(
            matches!(result, Err(HttpError::Unauthorized(challenge)) if challenge.scopes == ["files:read", "files:write"])
        );
        let (result, _) = fixture_request(wire_reply(
            "302 Found",
            "application/json",
            "Location: https://elsewhere.example/mcp\r\n",
            "",
        ))
        .await;
        assert_eq!(result, Err(HttpError::Redirect));
        let (result, _) = fixture_request(wire_reply(
            "200 OK",
            "application/json",
            "",
            &"x".repeat(MAX_REPLY_BYTES + 1),
        ))
        .await;
        assert_eq!(result, Err(HttpError::ResponseTooLarge));
        let unsupported =
            r#"{"jsonrpc":"2.0","id":7,"error":{"code":-32022,"message":"unsupported"}}"#;
        let (result, _) = fixture_request(wire_reply(
            "400 Bad Request",
            "application/json",
            "",
            unsupported,
        ))
        .await;
        assert_eq!(result, Err(HttpError::UnsupportedVersion));
    }

    #[tokio::test]
    async fn wire_fixture_cancellation_stops_a_pending_request() {
        let (url, peer) = wire_fixture(
            wire_reply("200 OK", "application/json", "", "{}"),
            Duration::from_secs(1),
        )
        .await;
        let cancel = CancellationToken::new();
        let cancelled = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            cancelled.cancel();
        });
        let result = send_prepared(
            &test_client(),
            url,
            HeaderMap::new(),
            b"{}".to_vec(),
            7,
            &cancel,
        )
        .await;
        assert_eq!(result, Err(HttpError::Cancelled));
        peer.abort();
    }

    #[test]
    fn header_annotations_are_reachable_primitive_unique_and_safe() {
        let schema = json!({"type":"object","properties":{
            "region":{"type":"string","x-mcp-header":"Region"},
            "nested":{"type":"object","properties":{"count":{"type":"integer","x-mcp-header":"Count"}}}
        }});
        let headers =
            parameter_headers(&schema, &json!({"region":"eu","nested":{"count":42}})).unwrap();
        assert_eq!(headers["mcp-param-region"], "eu");
        assert_eq!(headers["mcp-param-count"], "42");
        for bad in [
            json!({"type":"object","properties":{"a":{"type":"number","x-mcp-header":"A"}}}),
            json!({"type":"object","items":{"x-mcp-header":"A"}}),
            json!({"type":"object","properties":{"a":{"type":"string","x-mcp-header":"A"},"b":{"type":"string","x-mcp-header":"a"}}}),
            json!({"type":"object","properties":{"a":{"type":"string","x-mcp-header":"bad\rname"}}}),
        ] {
            assert_eq!(annotations(&bad), Err(HttpError::InvalidRequest));
        }
    }

    #[test]
    fn header_values_use_exact_base64_sentinel_rules() {
        assert_eq!(mirrored_value("plain").unwrap(), "plain");
        assert_eq!(mirrored_value(" café ").unwrap(), "=?base64?IGNhZsOpIA==?=");
        assert_eq!(
            mirrored_value("=?base64?literal?=").unwrap(),
            "=?base64?PT9iYXNlNjQ/bGl0ZXJhbD89?="
        );
        let schema =
            json!({"type":"object","properties":{"n":{"type":"integer","x-mcp-header":"N"}}});
        assert_eq!(
            parameter_headers(&schema, &json!({"n": SAFE_INTEGER + 1})),
            Err(HttpError::InvalidRequest)
        );
        assert!(parameter_headers(&schema, &json!({"n": null}))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn modern_headers_match_body_source_fields() {
        let params = Map::from_iter([
            ("name".into(), json!("écho")),
            ("arguments".into(), json!({"region":"west"})),
        ]);
        let schema = json!({"type":"object","properties":{"region":{"type":"string","x-mcp-header":"Region"}}});
        let headers = request_headers("tools/call", &params, Some(&schema)).unwrap();
        assert_eq!(headers["mcp-protocol-version"], protocol::MODERN_VERSION);
        assert_eq!(headers["mcp-method"], "tools/call");
        assert_eq!(headers["mcp-name"], "=?base64?w6ljaG8=?=");
        assert_eq!(headers["mcp-param-region"], "west");
    }

    #[test]
    fn json_and_request_scoped_sse_are_bounded_and_correlated() {
        let json_reply =
            br#"{"jsonrpc":"2.0","id":7,"result":{"resultType":"complete","tools":[]}}"#;
        assert_eq!(
            parse_reply("application/json; charset=utf-8", json_reply, 7).unwrap()["tools"],
            json!([])
        );
        let sse = b": keepalive\n\nevent: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\ndata: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"resultType\":\"complete\",\"tools\":[]}}\n\n";
        assert_eq!(
            parse_reply("text/event-stream", sse, 7).unwrap()["tools"],
            json!([])
        );
        let crlf = b"data: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"resultType\":\"complete\"}}\r\n\r\n";
        assert!(parse_reply("text/event-stream", crlf, 7).is_ok());
        assert_eq!(
            parse_reply("text/event-stream", sse, 8),
            Err(HttpError::InvalidResponse)
        );
        assert_eq!(
            parse_reply("text/event-stream", b"data: bad\n\n", 7),
            Err(HttpError::InvalidResponse)
        );
    }

    #[test]
    fn public_ip_policy_rejects_non_public_ranges() {
        for ip in [
            "127.0.0.1",
            "10.0.0.1",
            "169.254.2.2",
            "172.16.0.1",
            "192.168.1.1",
            "100.64.0.1",
            "198.51.100.1",
            "203.0.113.1",
            "::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(!public_ip(ip.parse().unwrap()), "{ip}");
        }
        assert!(public_ip("1.1.1.1".parse().unwrap()));
        assert!(public_ip("2606:4700:4700::1111".parse().unwrap()));
    }

    #[test]
    fn bearer_challenge_is_bounded_and_redacted() {
        let challenge = parse_challenge(Some(&HeaderValue::from_static(
            "Bearer resource_metadata=\"https://mcp.example.com/.well-known/oauth-protected-resource\", scope=\"files:read files:write\"",
        )));
        assert_eq!(challenge.scopes, ["files:read", "files:write"]);
        assert!(challenge.metadata_url.is_some());
        assert!(!format!("{challenge:?}").contains("files:read"));
    }
}
