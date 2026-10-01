//! Model routing for outbound MCP servers: live tool snapshots for the chat
//! registry, and the prepare/call pair behind every MCP tool the model uses.
//! A snapshot grants nothing. Each call re-resolves the active user's server,
//! re-lists its tools, checks the approved schema and consumes a one-shot
//! backend approval; nothing is replayed after an auth or version failure.

// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2024-2026 axpnet: AI-assisted (see AI-TRANSPARENCY.md)

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};

use futures_util::stream::{self, StreamExt};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{AppHandle, Webview};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use crate::mcp_client_bridge::{
    self as bridge, AdvertisedTool, BridgeError, BridgeRequest, Transport,
};
use crate::mcp_client_gate::GateRequest;

/// How long one server may take to start and list its tools.
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(20);
/// Servers listed at once.
const SNAPSHOT_CONCURRENCY: usize = 4;
/// A listing is reused only while its binding is unchanged and for this long.
const SNAPSHOT_TTL: Duration = Duration::from_secs(300);

/// Process-local key for effective and schema revisions. Revisions never
/// leave the process as anything but opaque digests.
fn revision_key() -> &'static Zeroizing<[u8; 32]> {
    static KEY: OnceLock<Zeroizing<[u8; 32]>> = OnceLock::new();
    KEY.get_or_init(|| {
        let mut key = Zeroizing::new([0; 32]);
        rand::rngs::OsRng.fill_bytes(&mut *key);
        key
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpTransport {
    Stdio,
    Http,
}
impl From<McpTransport> for Transport {
    fn from(transport: McpTransport) -> Self {
        match transport {
            McpTransport::Stdio => Transport::Stdio,
            McpTransport::Http => Transport::Http,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum McpHealth {
    Disabled,
    Ready,
    Error,
}

/// What the chat registry and the settings card see of one server.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerSnapshot {
    id: String,
    transport: McpTransport,
    enabled: bool,
    /// Effective revision of the server binding; empty when it did not resolve.
    revision: String,
    health: McpHealth,
    error_code: Option<&'static str>,
    tools: Vec<AdvertisedTool>,
    unsupported_tools: usize,
}

#[derive(Clone)]
struct Cached {
    tools: Vec<AdvertisedTool>,
    unsupported: usize,
    at: Instant,
}
type CacheKey = (i64, McpTransport, String, String);
static CACHE: LazyLock<Mutex<HashMap<CacheKey, Cached>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn main_window(webview: &Webview, what: &str) -> Result<(), &'static str> {
    crate::only_main_window(webview.label(), what).map_err(|_| "MCP_MAIN_WINDOW_REQUIRED")
}

/// Every server of the active user, enabled or not, by transport and ID.
fn catalog(app: &AppHandle) -> Result<Vec<(McpTransport, String, bool)>, &'static str> {
    let (conn, root_key, user_id) = crate::mcp_client_commands::context(app)?;
    let mut servers: Vec<_> = crate::mcp_client_commands::load(&conn, &root_key, user_id)?
        .into_iter()
        .map(|c| (McpTransport::Stdio, c.id, c.enabled))
        .collect();
    servers.extend(
        crate::mcp_client_http_commands::load(&conn, &root_key, user_id)?
            .into_iter()
            .map(|c| (McpTransport::Http, c.id, c.enabled)),
    );
    Ok(servers)
}

fn snapshot_of(
    id: String,
    transport: McpTransport,
    outcome: Result<(String, Cached), BridgeError>,
) -> McpServerSnapshot {
    match outcome {
        Ok((revision, listed)) => McpServerSnapshot {
            id,
            transport,
            enabled: true,
            revision,
            health: McpHealth::Ready,
            error_code: None,
            tools: listed.tools,
            unsupported_tools: listed.unsupported,
        },
        Err(error) => McpServerSnapshot {
            id,
            transport,
            enabled: true,
            revision: String::new(),
            health: McpHealth::Error,
            error_code: Some(error.code()),
            tools: Vec::new(),
            unsupported_tools: 0,
        },
    }
}

async fn list_server(
    app: &AppHandle,
    transport: McpTransport,
    id: &str,
    refresh: bool,
) -> Result<(String, Cached), BridgeError> {
    let key = revision_key();
    let (user_id, revision) = bridge::binding(app, key, transport.into(), id)?;
    let cache_key = (user_id, transport, id.to_owned(), revision.clone());
    if !refresh {
        let cached = CACHE
            .lock()
            .ok()
            .and_then(|cache| cache.get(&cache_key).cloned());
        if let Some(cached) = cached.filter(|c| c.at.elapsed() < SNAPSHOT_TTL) {
            return Ok((revision, cached));
        }
    }
    let cancel = CancellationToken::new();
    let discovery = tokio::time::timeout(
        SNAPSHOT_TIMEOUT,
        bridge::discover_tools(app, key, transport.into(), id, &cancel),
    )
    .await
    .map_err(|_| {
        cancel.cancel();
        BridgeError::Http(crate::mcp_client_http_transport::HttpError::Timeout)
    })??;
    let listed = Cached {
        tools: discovery.tools,
        unsupported: discovery.unsupported,
        at: Instant::now(),
    };
    if let Ok(mut cache) = CACHE.lock() {
        cache.insert(
            (
                discovery.user_id,
                transport,
                id.to_owned(),
                discovery.revision.clone(),
            ),
            listed.clone(),
        );
    }
    Ok((discovery.revision, listed))
}

/// Live health and validated tools of the active user's servers. A disabled
/// server is never started. `refresh` ignores listings still in the cache.
#[tauri::command]
pub async fn mcp_client_tool_snapshots(
    webview: Webview,
    app: AppHandle,
    refresh: Option<bool>,
) -> Result<Vec<McpServerSnapshot>, &'static str> {
    main_window(&webview, "mcp_client_tool_snapshots")?;
    let lookup = app.clone();
    let servers = tokio::task::spawn_blocking(move || catalog(&lookup))
        .await
        .map_err(|_| "MCP_STORE_UNAVAILABLE")??;
    let refresh = refresh.unwrap_or(false);
    let app = &app;
    let mut snapshots: Vec<McpServerSnapshot> = stream::iter(servers)
        .map(|(transport, id, enabled)| async move {
            if !enabled {
                return McpServerSnapshot {
                    id,
                    transport,
                    enabled: false,
                    revision: String::new(),
                    health: McpHealth::Disabled,
                    error_code: None,
                    tools: Vec::new(),
                    unsupported_tools: 0,
                };
            }
            let outcome = list_server(app, transport, &id, refresh).await;
            snapshot_of(id, transport, outcome)
        })
        .buffer_unordered(SNAPSHOT_CONCURRENCY)
        .collect()
        .await;
    snapshots.sort_by(|a, b| a.id.cmp(&b.id));
    if let Ok(mut cache) = CACHE.lock() {
        retain_listed(&mut cache, &snapshots);
    }
    Ok(snapshots)
}

/// Keeps only the listings an answer still stands on: a changed binding,
/// a disabled or removed server, or an error drops its listing.
fn retain_listed(cache: &mut HashMap<CacheKey, Cached>, snapshots: &[McpServerSnapshot]) {
    cache.retain(|(_, transport, id, revision), _| {
        snapshots.iter().any(|s| {
            s.health == McpHealth::Ready
                && s.transport == *transport
                && &s.id == id
                && &s.revision == revision
        })
    });
}

/// One model call of an MCP tool, named by the snapshot it was offered from.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpToolCall {
    transport: McpTransport,
    server_id: String,
    tool_name: String,
    arguments: Value,
    /// The snapshot's server revision: a changed binding is refused.
    expected_revision: String,
    /// The snapshot's schema revision for this tool: the approval binds it.
    expected_schema_revision: String,
    session_id: Option<String>,
    approval_grant_id: Option<String>,
}
impl McpToolCall {
    fn request(self) -> BridgeRequest {
        BridgeRequest {
            transport: self.transport.into(),
            expected_schema_revision: Some(self.expected_schema_revision),
            call: GateRequest {
                server_id: self.server_id,
                tool_name: self.tool_name,
                arguments: self.arguments,
                expected_revision: self.expected_revision,
                session_id: crate::ai_tools::cache_session_key(self.session_id.as_deref()),
                approval_grant_id: self.approval_grant_id,
            },
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolPreparation {
    approval_required: bool,
    request_id: Option<String>,
    allow_session_grant: bool,
}

/// Lists the server's tools under the live binding and opens a one-shot
/// approval request bound to the schema it found. Nothing runs here.
#[tauri::command]
pub async fn mcp_client_tool_prepare(
    webview: Webview,
    app: AppHandle,
    call: McpToolCall,
) -> Result<McpToolPreparation, &'static str> {
    main_window(&webview, "mcp_client_tool_prepare")?;
    if call.approval_grant_id.is_some() {
        return Err("MCP_CALL_INVALID_REQUEST");
    }
    let request = call.request();
    let cancel = CancellationToken::new();
    let prepared = bridge::prepare(&app, revision_key(), &request, &cancel)
        .await
        .map_err(|e| e.code())?;
    Ok(McpToolPreparation {
        approval_required: prepared.approval_required,
        request_id: prepared.request_id,
        allow_session_grant: prepared.allow_session_grant,
    })
}

/// Runs the approved call. The chat's Stop cancels it through `turn_id`.
#[tauri::command]
pub async fn mcp_client_tool_call(
    webview: Webview,
    app: AppHandle,
    call: McpToolCall,
    turn_id: Option<String>,
) -> Result<Value, &'static str> {
    main_window(&webview, "mcp_client_tool_call")?;
    if call.approval_grant_id.is_none() {
        return Err("MCP_APPROVAL_REQUIRED");
    }
    let request = call.request();
    let cancel = match turn_id.as_deref() {
        Some(turn) => crate::ai_tools::enter_turn_tool(turn).await,
        None => CancellationToken::new(),
    };
    let result = bridge::dispatch(&app, revision_key(), &request, &cancel).await;
    if let Some(turn) = turn_id.as_deref() {
        crate::ai_tools::leave_turn_tool(turn).await;
    }
    result.map_err(|e| e.code())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(value: Value) -> Result<McpToolCall, serde_json::Error> {
        serde_json::from_value(value)
    }
    fn valid() -> Value {
        json!({"transport":"http","serverId":"remote","toolName":"echo","arguments":{"text":"x"},
            "expectedRevision":"a".repeat(64),"expectedSchemaRevision":"b".repeat(64)})
    }

    #[test]
    fn a_call_names_its_snapshot_and_nothing_else() {
        let request = call(valid()).unwrap().request();
        assert_eq!(request.transport, Transport::Http);
        assert_eq!(
            request.expected_schema_revision.as_deref(),
            Some(&*"b".repeat(64))
        );
        assert_eq!(request.call.expected_revision, "a".repeat(64));
        assert_eq!(request.call.session_id, "__default__");
        assert!(request.call.approval_grant_id.is_none());
        let mut chat = valid();
        chat["sessionId"] = json!("chat-1");
        chat["approvalGrantId"] = json!("grant");
        let request = call(chat).unwrap().request();
        assert_eq!(request.call.session_id, "chat-1");
        assert_eq!(request.call.approval_grant_id.as_deref(), Some("grant"));
        // No frontend schema, annotation or danger claim can ride along.
        for extra in [
            "inputSchema",
            "dangerLevel",
            "annotations",
            "command",
            "endpoint",
        ] {
            let mut value = valid();
            value[extra] = json!({});
            assert!(call(value).is_err(), "{extra}");
        }
        let mut value = valid();
        value["expectedSchemaRevision"] = Value::Null;
        assert!(call(value).is_err());
        let mut value = valid();
        value["transport"] = json!("Stdio");
        assert!(call(value).is_err());
    }

    fn snapshot(id: &str, revision: &str, health: McpHealth) -> McpServerSnapshot {
        McpServerSnapshot {
            id: id.into(),
            transport: McpTransport::Stdio,
            enabled: health != McpHealth::Disabled,
            revision: revision.into(),
            health,
            error_code: None,
            tools: Vec::new(),
            unsupported_tools: 0,
        }
    }

    #[test]
    fn the_cache_keeps_only_listings_the_answer_stands_on() {
        let listed = Cached {
            tools: Vec::new(),
            unsupported: 0,
            at: Instant::now(),
        };
        let key =
            |id: &str, revision: &str| (1, McpTransport::Stdio, id.to_owned(), revision.to_owned());
        let mut cache: HashMap<CacheKey, Cached> = [
            key("kept", "r1"),
            key("rebound", "old"),
            key("disabled", "r1"),
            key("failing", "r1"),
            key("removed", "r1"),
        ]
        .into_iter()
        .map(|k| (k, listed.clone()))
        .collect();
        retain_listed(
            &mut cache,
            &[
                snapshot("kept", "r1", McpHealth::Ready),
                snapshot("rebound", "new", McpHealth::Ready),
                snapshot("disabled", "", McpHealth::Disabled),
                snapshot("failing", "r1", McpHealth::Error),
            ],
        );
        assert_eq!(cache.keys().collect::<Vec<_>>(), vec![&key("kept", "r1")]);
    }

    #[test]
    fn snapshots_serialize_without_server_claims() {
        let value = serde_json::to_value(snapshot_of(
            "local".into(),
            McpTransport::Stdio,
            Err(BridgeError::OAuthPending),
        ))
        .unwrap();
        assert_eq!(
            value,
            json!({"id":"local","transport":"stdio","enabled":true,"revision":"","health":"error",
                "errorCode":"MCP_OAUTH_REQUIRED","tools":[],"unsupportedTools":0})
        );
    }
}
