//! Pure outbound MCP STDIO message contract. Process supervision and dispatch
//! belong to the transport layer; peer-supplied text never enters errors.

// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2024-2026 axpnet: AI-assisted (see AI-TRANSPARENCY.md)

use serde_json::{json, Map, Value};

pub const MODERN_VERSION: &str = "2026-07-28";
pub const LEGACY_PREFERRED: &str = "2025-11-25";
pub const LEGACY_AEROFTP: &str = "2024-11-05";
const PROTOCOL_VERSION_KEY: &str = "io.modelcontextprotocol/protocolVersion";
const CLIENT_INFO_KEY: &str = "io.modelcontextprotocol/clientInfo";
const CLIENT_CAPABILITIES_KEY: &str = "io.modelcontextprotocol/clientCapabilities";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolError {
    InvalidRequest,
    InvalidReply,
    UnexpectedMessage,
    MismatchedId,
    UnsupportedVersion,
    RemoteError,
}

impl ProtocolError {
    /// Fixed diagnostics only. Never include peer JSON, error messages or secrets.
    pub const fn redacted(self) -> &'static str {
        match self {
            Self::InvalidRequest => "Invalid MCP client request",
            Self::InvalidReply => "Invalid MCP server reply",
            Self::UnexpectedMessage => "Unexpected MCP server message",
            Self::MismatchedId => "MCP reply ID mismatch",
            Self::UnsupportedVersion => "Unsupported MCP protocol version",
            Self::RemoteError => "MCP server returned an error",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Era {
    Modern,
    Legacy(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeVerdict {
    Modern,
    /// Start a fresh process and require a valid legacy initialize reply.
    LegacyHandshakeRequired,
}

/// These outcomes are valid only for the disposable `server/discover` sibling.
/// The transport must report malformed frames, cancellation and I/O errors
/// separately; neither a normal-session EOF nor a request timeout belongs here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeReply<'a> {
    Message(&'a Value),
    SilentProbeTimeout,
    ProbeChildExited,
}

fn valid_id(id: &Value) -> bool {
    id.as_str().is_some_and(|s| !s.is_empty()) || id.as_u64().is_some()
}

fn valid_identity(name: &str, version: &str) -> bool {
    !name.trim().is_empty() && !version.trim().is_empty()
}

/// Build a request without accepting caller-supplied metadata that could
/// override the negotiated version or capabilities.
pub fn request(
    era: Era,
    id: u64,
    method: &str,
    mut params: Map<String, Value>,
    client_name: &str,
    client_version: &str,
) -> Result<Value, ProtocolError> {
    if method.trim().is_empty()
        || id == 0
        || !valid_identity(client_name, client_version)
        || params.contains_key("_meta")
    {
        return Err(ProtocolError::InvalidRequest);
    }
    if matches!(era, Era::Modern) {
        let mut meta = Map::new();
        meta.insert(PROTOCOL_VERSION_KEY.into(), json!(MODERN_VERSION));
        meta.insert(
            CLIENT_INFO_KEY.into(),
            json!({"name": client_name, "version": client_version}),
        );
        meta.insert(CLIENT_CAPABILITIES_KEY.into(), json!({}));
        params.insert("_meta".into(), Value::Object(meta));
    }
    Ok(json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params}))
}

pub fn discover_request(
    id: u64,
    client_name: &str,
    client_version: &str,
) -> Result<Value, ProtocolError> {
    request(
        Era::Modern,
        id,
        "server/discover",
        Map::new(),
        client_name,
        client_version,
    )
}

pub fn initialize_request(
    id: u64,
    client_name: &str,
    client_version: &str,
) -> Result<Value, ProtocolError> {
    if id == 0 || !valid_identity(client_name, client_version) {
        return Err(ProtocolError::InvalidRequest);
    }
    Ok(json!({
        "jsonrpc":"2.0", "id":id, "method":"initialize",
        "params": {
            "protocolVersion": LEGACY_PREFERRED,
            "capabilities": {},
            "clientInfo": {"name":client_name, "version":client_version}
        }
    }))
}

pub fn initialized_notification() -> Value {
    json!({"jsonrpc":"2.0", "method":"notifications/initialized"})
}

pub enum Reply<'a> {
    Success(&'a Value),
    Error { code: i64, data: Option<&'a Value> },
}

/// Require a single correlated JSON-RPC reply. Server requests and unsolicited
/// notifications are not accepted by this request/reply-only contract.
pub fn reply(message: &Value, expected_id: u64) -> Result<Reply<'_>, ProtocolError> {
    let object = message.as_object().ok_or(ProtocolError::InvalidReply)?;
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(ProtocolError::InvalidReply);
    }
    if object.contains_key("method") {
        return Err(ProtocolError::UnexpectedMessage);
    }
    let id = object.get("id").ok_or(ProtocolError::UnexpectedMessage)?;
    if !valid_id(id) {
        return Err(ProtocolError::InvalidReply);
    }
    if id != &json!(expected_id) {
        return Err(ProtocolError::MismatchedId);
    }
    match (object.get("result"), object.get("error")) {
        (Some(result), None) if result.is_object() => Ok(Reply::Success(result)),
        (None, Some(error)) => {
            let error = error.as_object().ok_or(ProtocolError::InvalidReply)?;
            let code = error
                .get("code")
                .and_then(Value::as_i64)
                .ok_or(ProtocolError::InvalidReply)?;
            if error
                .get("message")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
            {
                return Err(ProtocolError::InvalidReply);
            }
            Ok(Reply::Error {
                code,
                data: error.get("data"),
            })
        }
        _ => Err(ProtocolError::InvalidReply),
    }
}

fn supported_versions(value: &Value, key: &str) -> Result<bool, ProtocolError> {
    let versions = value
        .get(key)
        .and_then(Value::as_array)
        .ok_or(ProtocolError::InvalidReply)?;
    if versions.is_empty()
        || !versions
            .iter()
            .all(|v| v.as_str().is_some_and(|s| !s.is_empty()))
    {
        return Err(ProtocolError::InvalidReply);
    }
    Ok(versions.iter().any(|v| v.as_str() == Some(MODERN_VERSION)))
}

/// The caller must pass only the response to a disposable sibling's probe.
/// A recognized modern version error is never interpreted as legacy.
pub fn classify_probe(
    observed: ProbeReply<'_>,
    expected_id: u64,
) -> Result<ProbeVerdict, ProtocolError> {
    let message = match observed {
        ProbeReply::Message(message) => message,
        ProbeReply::SilentProbeTimeout | ProbeReply::ProbeChildExited => {
            return Ok(ProbeVerdict::LegacyHandshakeRequired);
        }
    };
    match reply(message, expected_id)? {
        Reply::Success(result) => {
            if result.get("resultType").and_then(Value::as_str) != Some("complete")
                || result
                    .get("capabilities")
                    .and_then(Value::as_object)
                    .is_none()
                || result.get("ttlMs").and_then(Value::as_u64).is_none()
                || !matches!(
                    result.get("cacheScope").and_then(Value::as_str),
                    Some("public" | "private")
                )
            {
                return Err(ProtocolError::InvalidReply);
            }
            if supported_versions(result, "supportedVersions")? {
                Ok(ProbeVerdict::Modern)
            } else {
                Err(ProtocolError::UnsupportedVersion)
            }
        }
        Reply::Error { code: -32022, data } => {
            let data = data.ok_or(ProtocolError::InvalidReply)?;
            if data.get("requested").and_then(Value::as_str) != Some(MODERN_VERSION) {
                return Err(ProtocolError::InvalidReply);
            }
            if supported_versions(data, "supported")? {
                Ok(ProbeVerdict::Modern)
            } else {
                Err(ProtocolError::UnsupportedVersion)
            }
        }
        Reply::Error { code: -32601, .. } => Ok(ProbeVerdict::LegacyHandshakeRequired),
        Reply::Error { .. } => Err(ProtocolError::RemoteError),
    }
}

/// Called only after `LegacyHandshakeRequired`, on a fresh process. The
/// initialized notification may be sent only after this succeeds.
pub fn accept_legacy_initialize(message: &Value, expected_id: u64) -> Result<Era, ProtocolError> {
    let Reply::Success(result) = reply(message, expected_id)? else {
        return Err(ProtocolError::RemoteError);
    };
    let version = match result.get("protocolVersion").and_then(Value::as_str) {
        Some(LEGACY_PREFERRED) => LEGACY_PREFERRED,
        Some(LEGACY_AEROFTP) => LEGACY_AEROFTP,
        _ => return Err(ProtocolError::UnsupportedVersion),
    };
    if result
        .get("capabilities")
        .and_then(Value::as_object)
        .is_none()
        || result
            .get("serverInfo")
            .and_then(Value::as_object)
            .is_none_or(|info| {
                info.get("name")
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty)
                    || info
                        .get("version")
                        .and_then(Value::as_str)
                        .is_none_or(str::is_empty)
            })
    {
        return Err(ProtocolError::InvalidReply);
    }
    Ok(Era::Legacy(version))
}

/// Validate a generic RPC result shape before a method-specific parser sees it.
pub fn accept_result(message: &Value, expected_id: u64, era: Era) -> Result<&Value, ProtocolError> {
    let Reply::Success(result) = reply(message, expected_id)? else {
        return Err(ProtocolError::RemoteError);
    };
    if matches!(era, Era::Modern) && result.get("resultType").and_then(Value::as_str).is_none() {
        return Err(ProtocolError::InvalidReply);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn discover(id: u64, versions: Value) -> Value {
        json!({"jsonrpc":"2.0","id":id,"result":{"resultType":"complete","supportedVersions":versions,"capabilities":{},"ttlMs":0,"cacheScope":"private"}})
    }

    #[test]
    fn modern_request_carries_metadata_on_every_call() {
        for method in ["server/discover", "tools/list", "tools/call"] {
            let value = request(Era::Modern, 7, method, Map::new(), "AeroFTP", "4.2").unwrap();
            assert_eq!(
                value["params"]["_meta"][PROTOCOL_VERSION_KEY],
                MODERN_VERSION
            );
            assert_eq!(value["params"]["_meta"][CLIENT_CAPABILITIES_KEY], json!({}));
            assert_eq!(value["params"]["_meta"][CLIENT_INFO_KEY]["name"], "AeroFTP");
        }
        let mut params = Map::new();
        params.insert("_meta".into(), json!({"fake":"override"}));
        assert!(matches!(
            request(Era::Modern, 1, "tools/list", params, "AeroFTP", "4.2"),
            Err(ProtocolError::InvalidRequest)
        ));
    }

    #[test]
    fn discovery_requires_valid_version_overlap() {
        assert!(matches!(
            classify_probe(
                ProbeReply::Message(&discover(1, json!([MODERN_VERSION]))),
                1
            ),
            Ok(ProbeVerdict::Modern)
        ));
        assert!(matches!(
            classify_probe(ProbeReply::Message(&discover(1, json!(["2027-01-01"]))), 1),
            Err(ProtocolError::UnsupportedVersion)
        ));
        assert!(matches!(
            classify_probe(ProbeReply::Message(&discover(1, json!([42]))), 1),
            Err(ProtocolError::InvalidReply)
        ));
        assert!(matches!(
            classify_probe(
                ProbeReply::Message(
                    &json!({"jsonrpc":"2.0","id":1,"result":{"supportedVersions":[MODERN_VERSION]}})
                ),
                1
            ),
            Err(ProtocolError::InvalidReply)
        ));
    }

    #[test]
    fn recognized_modern_error_never_falls_back() {
        let modern = json!({"jsonrpc":"2.0","id":1,"error":{"code":-32022,"message":"unsupported","data":{"requested":MODERN_VERSION,"supported":[MODERN_VERSION]}}});
        assert!(matches!(
            classify_probe(ProbeReply::Message(&modern), 1),
            Ok(ProbeVerdict::Modern)
        ));
        let incompatible = json!({"jsonrpc":"2.0","id":1,"error":{"code":-32022,"message":"unsupported","data":{"requested":MODERN_VERSION,"supported":["2027-01-01"]}}});
        assert!(matches!(
            classify_probe(ProbeReply::Message(&incompatible), 1),
            Err(ProtocolError::UnsupportedVersion)
        ));
        let malformed = json!({"jsonrpc":"2.0","id":1,"error":{"code":-32022,"message":"unsupported","data":{"supported":[MODERN_VERSION]}}});
        assert!(matches!(
            classify_probe(ProbeReply::Message(&malformed), 1),
            Err(ProtocolError::InvalidReply)
        ));
    }

    #[test]
    fn remote_probe_errors_do_not_trigger_legacy_handshake() {
        for code in [-32602, -32603, -32000, 42] {
            let message =
                json!({"jsonrpc":"2.0","id":1,"error":{"code":code,"message":"remote failure"}});
            assert_eq!(
                classify_probe(ProbeReply::Message(&message), 1),
                Err(ProtocolError::RemoteError)
            );
        }
    }

    #[test]
    fn legacy_requires_explicit_initialize_on_fresh_session() {
        let unknown_method =
            json!({"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"unknown"}});
        assert!(matches!(
            classify_probe(ProbeReply::Message(&unknown_method), 1),
            Ok(ProbeVerdict::LegacyHandshakeRequired)
        ));
        assert_eq!(
            classify_probe(ProbeReply::SilentProbeTimeout, 1),
            Ok(ProbeVerdict::LegacyHandshakeRequired)
        );
        assert_eq!(
            classify_probe(ProbeReply::ProbeChildExited, 1),
            Ok(ProbeVerdict::LegacyHandshakeRequired)
        );
        let init = initialize_request(2, "AeroFTP", "4.2").unwrap();
        assert_eq!(init["params"]["protocolVersion"], LEGACY_PREFERRED);
        assert!(init["params"].get("_meta").is_none());
        for version in [LEGACY_PREFERRED, LEGACY_AEROFTP] {
            let response = json!({"jsonrpc":"2.0","id":2,"result":{"protocolVersion":version,"capabilities":{},"serverInfo":{"name":"peer","version":"1"}}});
            assert_eq!(
                accept_legacy_initialize(&response, 2),
                Ok(Era::Legacy(version))
            );
        }
        let unsupported = json!({"jsonrpc":"2.0","id":2,"result":{"protocolVersion":"2025-03-26","capabilities":{},"serverInfo":{"name":"peer","version":"1"}}});
        assert_eq!(
            accept_legacy_initialize(&unsupported, 2),
            Err(ProtocolError::UnsupportedVersion)
        );
        assert_eq!(
            initialized_notification()["method"],
            "notifications/initialized"
        );
    }

    #[test]
    fn malformed_and_unsolicited_messages_fail_closed() {
        let wrong_id = json!({"jsonrpc":"2.0","id":9,"result":{}});
        assert!(matches!(
            reply(&wrong_id, 1),
            Err(ProtocolError::MismatchedId)
        ));
        let server_request =
            json!({"jsonrpc":"2.0","id":1,"method":"sampling/createMessage","params":{}});
        assert!(matches!(
            reply(&server_request, 1),
            Err(ProtocolError::UnexpectedMessage)
        ));
        for message in [
            json!({"jsonrpc":"2.0","id":1,"result":{},"error":{"code":-1,"message":"bad"}}),
            json!({"jsonrpc":"2.0","id":1,"error":{"code":"-1","message":"bad"}}),
            json!({"jsonrpc":"2.0","id":1,"error":{"code":-1}}),
            json!({"jsonrpc":"2.0","id":1,"result":null}),
            json!({"jsonrpc":"2.0","method":"notifications/message","params":{}}),
        ] {
            assert!(reply(&message, 1).is_err());
        }
        let result = json!({"jsonrpc":"2.0","id":1,"result":{}});
        assert_eq!(
            accept_result(&result, 1, Era::Modern).err(),
            Some(ProtocolError::InvalidReply)
        );
        assert!(accept_result(&result, 1, Era::Legacy(LEGACY_PREFERRED)).is_ok());
        assert_eq!(
            ProtocolError::RemoteError.redacted(),
            "MCP server returned an error"
        );
    }
}
