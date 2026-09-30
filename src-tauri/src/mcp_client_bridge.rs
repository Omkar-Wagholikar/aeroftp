//! Private backend bridge. No Tauri command, frontend schema, or model route.
//! Fresh discovery is not a grant: every tools/call consumes backend approval.
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::ai_tools::{self, AiToolApprovalPreparation};
use crate::mcp_client_commands;
use crate::mcp_client_config::{McpServerConfig, ResolvedMcpEnvironment};
use crate::mcp_client_gate::{GateError, GateRequest};
use crate::mcp_client_http_config::{McpHttpAuth, McpHttpServerConfig, ResolvedMcpHttpAuth};
use crate::mcp_client_http_transport::{self, HttpError};
use crate::mcp_client_schema::{self as schema, SchemaError};
use crate::mcp_client_transport::{Limits, StdioSupervisor, TransportError};
use serde_json::{json, Map, Value};
use tauri::AppHandle;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Transport {
    Stdio,
    Http,
}
impl Transport {
    fn label(self) -> &'static str {
        match self {
            Self::Stdio => "stdio",
            Self::Http => "http",
        }
    }
}
pub(crate) struct BridgeRequest {
    pub call: GateRequest,
    pub transport: Transport,
    /// Backend schema revision returned by prepare, never a frontend schema.
    pub expected_schema_revision: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BridgeError {
    Gate(GateError),
    Schema(SchemaError),
    Stdio(TransportError),
    Http(HttpError),
    SchemaChanged,
    OAuthPending,
}
impl From<GateError> for BridgeError {
    fn from(e: GateError) -> Self {
        Self::Gate(e)
    }
}
impl From<SchemaError> for BridgeError {
    fn from(e: SchemaError) -> Self {
        Self::Schema(e)
    }
}
impl From<TransportError> for BridgeError {
    fn from(e: TransportError) -> Self {
        Self::Stdio(e)
    }
}
impl From<HttpError> for BridgeError {
    fn from(e: HttpError) -> Self {
        Self::Http(e)
    }
}
pub(crate) struct Preparation {
    pub approval: AiToolApprovalPreparation,
    pub schema_revision: String,
}
struct State {
    user_id: i64,
    revision: String,
    config: Config,
}
enum Config {
    Stdio(McpServerConfig, ResolvedMcpEnvironment),
    Http(McpHttpServerConfig, ResolvedMcpHttpAuth),
}

/// HTTP settings are backend-only. OAuth stays unavailable until its lifecycle
/// can rebind tokens to live issuer/resource/user state in the next slice.
fn resolve(app: &AppHandle, key: &[u8; 32], request: &BridgeRequest) -> Result<State, BridgeError> {
    validate_request(request)?;
    let (conn, root_key, user_id) =
        mcp_client_commands::context(app).map_err(|_| GateError::UserUnavailable)?;
    let config = match request.transport {
        Transport::Stdio => {
            let config = mcp_client_commands::load(&conn, &root_key, user_id)
                .map_err(|_| GateError::ConfigUnavailable)?
                .into_iter()
                .find(|c| c.id == request.call.server_id)
                .ok_or(GateError::ConfigUnavailable)?;
            if !config.enabled {
                return Err(GateError::ConfigDisabled.into());
            }
            let environment = config
                .resolve_with(key, user_id, |account| {
                    crate::user_partitions::get_user_credential_for(
                        &conn, &root_key, user_id, account,
                    )
                    .map_err(|_| ())?
                    .ok_or(())
                })
                .map_err(|_| GateError::SecretUnavailable)?;
            Config::Stdio(config, environment)
        }
        Transport::Http => {
            let value = crate::user_partitions::get_user_setting_for(
                &conn,
                &root_key,
                user_id,
                "aeroagent_mcp_http_servers",
            )
            .map_err(|_| GateError::ConfigUnavailable)?
            .unwrap_or(json!([]));
            let configs: Vec<McpHttpServerConfig> =
                serde_json::from_value(value).map_err(|_| GateError::ConfigUnavailable)?;
            let mut ids = std::collections::HashSet::new();
            if configs.len() > 32
                || configs
                    .iter()
                    .any(|c| c.validate().is_err() || !ids.insert(c.id.clone()))
            {
                return Err(GateError::ConfigUnavailable.into());
            }
            let config = configs
                .into_iter()
                .find(|c| c.id == request.call.server_id)
                .ok_or(GateError::ConfigUnavailable)?;
            if !config.enabled {
                return Err(GateError::ConfigDisabled.into());
            }
            if matches!(config.auth, McpHttpAuth::OAuth { .. }) {
                return Err(BridgeError::OAuthPending);
            }
            let auth = config
                .resolve_from_active_user(&conn, &root_key, key)
                .map_err(|_| GateError::SecretUnavailable)?;
            Config::Http(config, auth)
        }
    };
    let revision = match &config {
        Config::Stdio(_, e) => e.effective_revision.clone(),
        Config::Http(_, a) => a.effective_revision.clone(),
    };
    if revision != request.call.expected_revision {
        return Err(GateError::StaleRevision.into());
    }
    Ok(State {
        user_id,
        revision,
        config,
    })
}

trait Peer {
    async fn call(
        &mut self,
        method: &str,
        params: Map<String, Value>,
        schema: Option<&Value>,
        cancel: &CancellationToken,
    ) -> Result<Value, BridgeError>;
}
enum Connection {
    Stdio(Box<StdioSupervisor>),
    Http {
        config: McpHttpServerConfig,
        auth: ResolvedMcpHttpAuth,
        id: u64,
    },
}
impl Peer for Connection {
    async fn call(
        &mut self,
        method: &str,
        params: Map<String, Value>,
        schema: Option<&Value>,
        cancel: &CancellationToken,
    ) -> Result<Value, BridgeError> {
        match self {
            Self::Stdio(peer) => Ok(peer.call(method, params, cancel).await?),
            Self::Http { config, auth, id } => {
                *id = id.checked_add(1).ok_or(HttpError::InvalidRequest)?;
                Ok(mcp_client_http_transport::request(
                    config,
                    method,
                    params,
                    schema,
                    auth.bearer.as_deref().map(String::as_str),
                    *id,
                    cancel,
                )
                .await?)
            }
        }
    }
}
impl Connection {
    async fn connect(
        config: Config,
        cancel: &CancellationToken,
        fresh: impl FnMut() -> Result<(), BridgeError>,
    ) -> Result<Self, BridgeError> {
        check_cancel(cancel)?;
        match config {
            Config::Stdio(config, env) => {
                let mut peer =
                    StdioSupervisor::connect_checked(config, env, Limits::default(), cancel, fresh)
                        .await?;
                peer.disable_restart();
                Ok(Self::Stdio(Box::new(peer)))
            }
            Config::Http(config, auth) => Ok(Self::Http {
                config,
                auth,
                id: 0,
            }),
        }
    }
    async fn shutdown(&mut self) {
        if let Self::Stdio(peer) = self {
            peer.shutdown().await;
        }
    }
}
fn validate_request(request: &BridgeRequest) -> Result<(), BridgeError> {
    request.call.validate()?;
    if request
        .expected_schema_revision
        .as_ref()
        .is_some_and(|r| r.len() != 64 || !r.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err(GateError::InvalidRequest.into());
    }
    Ok(())
}
fn check_cancel(cancel: &CancellationToken) -> Result<(), BridgeError> {
    if cancel.is_cancelled() {
        return Err(TransportError::Cancelled.into());
    }
    Ok(())
}
fn scope(
    request: &BridgeRequest,
    user_id: i64,
    key: &[u8; 32],
    schema_revision: &str,
) -> Result<String, BridgeError> {
    let args = request.call.validate()?;
    Ok(
        json!({"kind":"mcp_client_bridge_v1","transport":request.transport.label(),
        "user_id":user_id,"server_id":request.call.server_id,"tool_name":request.call.tool_name,
        "effective_revision":request.call.expected_revision,"schema_revision":schema_revision,
        "arguments_digest":blake3::keyed_hash(key,&args).to_hex().to_string()})
        .to_string(),
    )
}

/// Re-list after authorization so a schema changed during approval cannot execute.
#[allow(clippy::too_many_arguments)] // Explicit security context and injectable test boundaries.
async fn run<P, F, A, Fut>(
    peer: &mut P,
    request: &BridgeRequest,
    key: &[u8; 32],
    user_id: i64,
    mut fresh: F,
    approve: A,
    execute: bool,
    cancel: &CancellationToken,
) -> Result<(String, Option<Value>), BridgeError>
where
    P: Peer,
    F: FnMut() -> Result<(), BridgeError>,
    A: FnOnce(String, String) -> Fut,
    Fut: std::future::Future<Output = Result<(), BridgeError>>,
{
    validate_request(request)?;
    fresh()?;
    check_cancel(cancel)?;
    let listed = peer.call("tools/list", Map::new(), None, cancel).await?;
    fresh()?;
    check_cancel(cancel)?;
    let input = schema::discover(&listed, &request.call.tool_name)?;
    schema::validate_arguments(&input, &request.call.arguments)?;
    let revision = schema::revision(&input, key);
    if request
        .expected_schema_revision
        .as_ref()
        .is_some_and(|r| r != &revision)
        || execute && request.expected_schema_revision.is_none()
    {
        return Err(BridgeError::SchemaChanged);
    }
    let approval_key = scope(request, user_id, key, &revision)?;
    approve(approval_key, revision.clone()).await?;
    fresh()?;
    check_cancel(cancel)?;
    if !execute {
        return Ok((revision, None));
    }
    let listed = peer.call("tools/list", Map::new(), None, cancel).await?;
    fresh()?;
    check_cancel(cancel)?;
    let latest = schema::discover(&listed, &request.call.tool_name)?;
    if schema::revision(&latest, key) != revision {
        return Err(BridgeError::SchemaChanged);
    }
    schema::validate_arguments(&latest, &request.call.arguments)?;
    let mut params = Map::new();
    params.insert("name".into(), json!(request.call.tool_name));
    params.insert("arguments".into(), request.call.arguments.clone());
    fresh()?;
    check_cancel(cancel)?;
    let result = peer
        .call("tools/call", params, Some(&latest), cancel)
        .await?;
    fresh()?;
    check_cancel(cancel)?;
    Ok((revision, Some(result)))
}

async fn with_backend(
    app: &AppHandle,
    key: &[u8; 32],
    request: &BridgeRequest,
    execute: bool,
    cancel: &CancellationToken,
) -> Result<(String, Option<Value>, Option<AiToolApprovalPreparation>), BridgeError> {
    let started = std::time::Instant::now();
    let mut audit_user = None;
    let result = async {
        check_cancel(cancel)?;
        let state = resolve(app, key, request)?;
        let user_id = state.user_id;
        audit_user = Some(user_id);
        let revision = state.revision;
        let mut peer = Connection::connect(state.config, cancel, || {
            let current = resolve(app, key, request)?;
            if current.user_id != user_id || current.revision != revision {
                return Err(GateError::StaleRevision.into());
            }
            Ok(())
        })
        .await?;
        let mut preparation = None;
        let preparation_slot = &mut preparation;
        let outcome = run(
            &mut peer,
            request,
            key,
            user_id,
            || {
                let current = resolve(app, key, request)?;
                if current.user_id != user_id || current.revision != revision {
                    return Err(GateError::StaleRevision.into());
                }
                Ok(())
            },
            |approval_key, _| async move {
                let tool = format!(
                    "mcp:{}:{}:{}",
                    request.transport.label(),
                    request.call.server_id,
                    request.call.tool_name
                );
                if execute {
                    ai_tools::ensure_ai_tool_approval(
                        Some(&request.call.session_id),
                        &tool,
                        &approval_key,
                        &approval_key,
                        request.call.approval_grant_id.as_deref(),
                    )
                    .await
                    .map_err(|_| GateError::ApprovalRequired)?;
                } else {
                    *preparation_slot = Some(
                        ai_tools::prepare_backend_approval_request(
                            Some(&request.call.session_id),
                            &tool,
                            approval_key.clone(),
                            approval_key,
                            false,
                            format!(
                                "AeroAgent wants to: Run MCP Tool\n\n  server: {}\n  tool: {}",
                                request.call.server_id, request.call.tool_name
                            ),
                        )
                        .await,
                    );
                }
                Ok(())
            },
            execute,
            cancel,
        )
        .await;
        peer.shutdown().await;
        let (schema_revision, value) = outcome?;
        Ok((schema_revision, value, preparation))
    }
    .await;
    let status = match &result {
        Ok(_) => {
            if execute {
                "success"
            } else {
                "prepared"
            }
        }
        Err(BridgeError::Gate(GateError::StaleRevision) | BridgeError::SchemaChanged) => "stale",
        Err(BridgeError::Gate(GateError::ApprovalRequired)) => "denied",
        Err(
            BridgeError::Stdio(TransportError::Cancelled) | BridgeError::Http(HttpError::Cancelled),
        ) => "cancelled",
        Err(_) => "rejected",
    };
    tracing::info!(target: "mcp_client_audit", user_id = audit_user.unwrap_or_default(),
        transport = request.transport.label(),
        server_digest = %blake3::keyed_hash(key,request.call.server_id.as_bytes()).to_hex(),
        tool_digest = %blake3::keyed_hash(key,request.call.tool_name.as_bytes()).to_hex(),
        revision_digest = %blake3::keyed_hash(key,request.call.expected_revision.as_bytes()).to_hex(),
        schema_digest = %blake3::keyed_hash(key, result.as_ref().map(|(r,_,_)| r.as_bytes()).unwrap_or_else(|_| request.expected_schema_revision.as_deref().unwrap_or("").as_bytes())).to_hex(),
        arguments_digest = %request.call.validate().map(|args|blake3::keyed_hash(key,&args).to_hex().to_string()).unwrap_or_default(),
        status, duration_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64, "private MCP bridge");
    result
}
pub(crate) async fn prepare(
    app: &AppHandle,
    key: &[u8; 32],
    request: &BridgeRequest,
    cancel: &CancellationToken,
) -> Result<Preparation, BridgeError> {
    let (schema_revision, _, approval) = with_backend(app, key, request, false, cancel).await?;
    Ok(Preparation {
        schema_revision,
        approval: approval.ok_or(GateError::ApprovalRequired)?,
    })
}
pub(crate) async fn dispatch(
    app: &AppHandle,
    key: &[u8; 32],
    request: &BridgeRequest,
    cancel: &CancellationToken,
) -> Result<Value, BridgeError> {
    let (_, value, _) = with_backend(app, key, request, true, cancel).await?;
    value.ok_or(GateError::InvalidRequest.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    const KEY: [u8; 32] = [41; 32];
    fn input() -> Value {
        json!({"type":"object","properties":{"text":{"type":"string","x-mcp-header":"Text"}},"required":["text"],"additionalProperties":false})
    }
    fn request() -> BridgeRequest {
        BridgeRequest {
            transport: Transport::Http,
            expected_schema_revision: Some(schema::revision(&input(), &KEY)),
            call: GateRequest {
                server_id: "fixture".into(),
                tool_name: "echo".into(),
                arguments: json!({"text":"private argument"}),
                expected_revision: "a".repeat(64),
                session_id: "chat-fixture".into(),
                approval_grant_id: None,
            },
        }
    }
    struct Fixture {
        calls: Vec<String>,
        schemas: Vec<Value>,
        failure: Option<BridgeError>,
        cancel_after_call: bool,
    }
    impl Fixture {
        fn new() -> Self {
            Self {
                calls: vec![],
                schemas: vec![input(), input()],
                failure: None,
                cancel_after_call: false,
            }
        }
    }
    impl Peer for Fixture {
        async fn call(
            &mut self,
            method: &str,
            params: Map<String, Value>,
            supplied: Option<&Value>,
            cancel: &CancellationToken,
        ) -> Result<Value, BridgeError> {
            self.calls.push(method.into());
            if let Some(error) = self.failure.take() {
                return Err(error);
            }
            if method == "tools/list" {
                assert!(supplied.is_none());
                return Ok(json!({"tools":[{"name":"echo","inputSchema":self.schemas.remove(0)}]}));
            }
            assert_eq!(supplied, Some(&input()));
            assert_eq!(params["arguments"], json!({"text":"private argument"}));
            if self.cancel_after_call {
                cancel.cancel();
            }
            Ok(json!({"content":[{"type":"text","text":"reply"}]}))
        }
    }
    #[tokio::test]
    async fn fresh_schema_binds_approval_and_supplies_call_headers() {
        let mut peer = Fixture::new();
        let checks = Cell::new(0);
        let (_, result) = run(
            &mut peer,
            &request(),
            &KEY,
            1,
            || {
                checks.set(checks.get() + 1);
                Ok(())
            },
            |scope, revision| async move {
                assert!(!scope.contains("private argument"));
                assert!(scope.contains("http"));
                assert!(scope.contains(&revision));
                Ok(())
            },
            true,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(result.unwrap().get("content").is_some());
        assert_eq!(peer.calls, ["tools/list", "tools/list", "tools/call"]);
        assert_eq!(checks.get(), 6);
    }
    #[tokio::test]
    async fn denied_approval_never_calls_tool() {
        let mut peer = Fixture::new();
        let result = run(
            &mut peer,
            &request(),
            &KEY,
            1,
            || Ok(()),
            |_, _| async { Err(GateError::ApprovalRequired.into()) },
            true,
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(result, Err(BridgeError::Gate(GateError::ApprovalRequired)));
        assert_eq!(peer.calls, ["tools/list"]);
    }
    #[tokio::test]
    async fn schema_change_during_approval_never_calls_tool() {
        let mut peer = Fixture::new();
        peer.schemas[1]["properties"]["text"]["x-mcp-header"] = json!("Other");
        let result = run(
            &mut peer,
            &request(),
            &KEY,
            1,
            || Ok(()),
            |_, _| async { Ok(()) },
            true,
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(result, Err(BridgeError::SchemaChanged));
        assert_eq!(peer.calls, ["tools/list", "tools/list"]);
    }
    #[tokio::test]
    async fn stale_snapshot_and_invalid_arguments_fail_before_approval() {
        for variant in ["schema", "arguments", "missing"] {
            let mut req = request();
            match variant {
                "schema" => req.expected_schema_revision = Some("b".repeat(64)),
                "arguments" => req.call.arguments = json!({"text":7}),
                _ => req.expected_schema_revision = None,
            }
            let mut peer = Fixture::new();
            let result = run(
                &mut peer,
                &req,
                &KEY,
                1,
                || Ok(()),
                |_, _| async { panic!("approval not reachable") },
                true,
                &CancellationToken::new(),
            )
            .await;
            assert!(result.is_err());
            assert_eq!(peer.calls, ["tools/list"]);
        }
    }
    #[tokio::test]
    async fn live_state_rejection_at_each_boundary_suppresses_calls_or_result() {
        for boundary in 1..=6 {
            for error in [
                GateError::ConfigDisabled,
                GateError::StaleRevision,
                GateError::UserUnavailable,
                GateError::SecretUnavailable,
            ] {
                let checks = Cell::new(0);
                let mut peer = Fixture::new();
                let result = run(
                    &mut peer,
                    &request(),
                    &KEY,
                    1,
                    || {
                        checks.set(checks.get() + 1);
                        if checks.get() == boundary {
                            Err(error.into())
                        } else {
                            Ok(())
                        }
                    },
                    |_, _| async { Ok(()) },
                    true,
                    &CancellationToken::new(),
                )
                .await;
                assert_eq!(result, Err(BridgeError::Gate(error)));
                assert_eq!(
                    peer.calls
                        .iter()
                        .filter(|m| m.as_str() == "tools/call")
                        .count(),
                    usize::from(boundary == 6)
                );
            }
        }
    }
    #[tokio::test]
    async fn cancellation_before_approval_after_approval_and_after_call() {
        for phase in ["before", "approval", "result"] {
            let cancel = CancellationToken::new();
            let mut peer = Fixture::new();
            if phase == "before" {
                cancel.cancel();
            }
            peer.cancel_after_call = phase == "result";
            let result = run(
                &mut peer,
                &request(),
                &KEY,
                1,
                || Ok(()),
                |_, _| async {
                    if phase == "approval" {
                        cancel.cancel();
                    }
                    Ok(())
                },
                true,
                &cancel,
            )
            .await;
            assert_eq!(result, Err(BridgeError::Stdio(TransportError::Cancelled)));
            assert_eq!(
                peer.calls
                    .iter()
                    .filter(|m| m.as_str() == "tools/call")
                    .count(),
                usize::from(phase == "result")
            );
        }
    }
    #[tokio::test]
    async fn auth_challenge_fails_without_approval_or_retry() {
        let mut peer = Fixture::new();
        let error = BridgeError::Http(HttpError::Unauthorized(
            mcp_client_http_transport::AuthChallenge {
                metadata_url: None,
                scopes: vec![],
            },
        ));
        peer.failure = Some(error.clone());
        assert_eq!(
            run(
                &mut peer,
                &request(),
                &KEY,
                1,
                || Ok(()),
                |_, _| async { panic!("no approval") },
                true,
                &CancellationToken::new()
            )
            .await,
            Err(error)
        );
        assert_eq!(peer.calls, ["tools/list"]);
    }
    #[tokio::test]
    async fn preparation_discovers_but_never_executes() {
        let mut req = request();
        req.expected_schema_revision = None;
        let mut peer = Fixture::new();
        let (revision, value) = run(
            &mut peer,
            &req,
            &KEY,
            1,
            || Ok(()),
            |_, _| async { Ok(()) },
            false,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(revision, schema::revision(&input(), &KEY));
        assert!(value.is_none());
        assert_eq!(peer.calls, ["tools/list"]);
    }
    #[test]
    fn approval_scope_separates_transport_user_schema_config_arguments_and_tool() {
        let base = scope(&request(), 1, &KEY, "schema-a").unwrap();
        for field in ["transport", "tool", "server", "config", "args"] {
            let mut req = request();
            match field {
                "transport" => req.transport = Transport::Stdio,
                "tool" => req.call.tool_name = "other".into(),
                "server" => req.call.server_id = "other".into(),
                "config" => req.call.expected_revision = "b".repeat(64),
                _ => req.call.arguments = json!({"text":"other"}),
            }
            assert_ne!(scope(&req, 1, &KEY, "schema-a").unwrap(), base);
        }
        assert_ne!(scope(&request(), 2, &KEY, "schema-a").unwrap(), base);
        assert_ne!(scope(&request(), 1, &KEY, "schema-b").unwrap(), base);
    }
}

#[cfg(all(test, target_os = "linux"))]
mod wire_tests {
    use super::*;
    use std::collections::BTreeMap;
    #[tokio::test]
    async fn isolated_stdio_bridge_round_trips_modern_and_legacy_fixture() {
        let node = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|p| p.join("node"))
            .find(|p| p.is_file())
            .unwrap();
        let script = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/mcp_stdio_fixture.mjs");
        let input = json!({"type":"object","properties":{"text":{"type":"string"}}});
        for mode in ["modern", "legacy-new"] {
            let key = [73; 32];
            let cancel = CancellationToken::new();
            let config = McpServerConfig {
                id: "fixture".into(),
                command: node.to_string_lossy().into_owned(),
                args: vec![script.to_string_lossy().into_owned(), mode.into()],
                env: BTreeMap::new(),
                enabled: true,
                revision: 1,
            };
            let env = ResolvedMcpEnvironment {
                effective_revision: "a".repeat(64),
                vars: BTreeMap::new(),
            };
            let request = BridgeRequest {
                transport: Transport::Stdio,
                expected_schema_revision: Some(schema::revision(&input, &key)),
                call: GateRequest {
                    server_id: "fixture".into(),
                    tool_name: "echo".into(),
                    arguments: json!({"text":"bridge wire reply"}),
                    expected_revision: "a".repeat(64),
                    session_id: "bridge-wire".into(),
                    approval_grant_id: None,
                },
            };
            let mut peer = Connection::connect(Config::Stdio(config, env), &cancel, || Ok(()))
                .await
                .unwrap();
            let result = run(
                &mut peer,
                &request,
                &key,
                1,
                || Ok(()),
                |_, _| async { Ok(()) },
                true,
                &cancel,
            )
            .await;
            peer.shutdown().await;
            assert_eq!(
                result.unwrap().1.unwrap()["content"][0]["text"],
                "bridge wire reply"
            );
        }
    }
}

#[cfg(test)]
mod http_wire_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    struct Wire {
        url: url::Url,
        id: u64,
    }
    impl Peer for Wire {
        async fn call(
            &mut self,
            method: &str,
            params: Map<String, Value>,
            schema: Option<&Value>,
            cancel: &CancellationToken,
        ) -> Result<Value, BridgeError> {
            self.id += 1;
            Ok(mcp_client_http_transport::fixture_exchange(
                self.url.clone(),
                method,
                params,
                schema,
                self.id,
                cancel,
            )
            .await?)
        }
    }
    #[tokio::test]
    async fn backend_discovered_header_is_mirrored_on_real_http_wire() {
        let input = json!({"type":"object","properties":{"text":{"type":"string","x-mcp-header":"Fresh"}},"required":["text"],"additionalProperties":false});
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let response_schema = input.clone();
        let server = tokio::spawn(async move {
            for id in 1..=3 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = vec![];
                let mut buf = [0; 4096];
                let (headers, body) = loop {
                    let read = stream.read(&mut buf).await.unwrap();
                    assert!(read > 0);
                    bytes.extend_from_slice(&buf[..read]);
                    if let Some(split) = bytes.windows(4).position(|b| b == b"\r\n\r\n") {
                        let headers = String::from_utf8(bytes[..split].to_vec()).unwrap();
                        let len = headers
                            .lines()
                            .find_map(|l| {
                                l.split_once(':')
                                    .filter(|(n, _)| n.eq_ignore_ascii_case("content-length"))
                                    .map(|(_, v)| v.trim().parse::<usize>().unwrap())
                            })
                            .unwrap();
                        if bytes.len() >= split + 4 + len {
                            break (
                                headers,
                                serde_json::from_slice::<Value>(&bytes[split + 4..split + 4 + len])
                                    .unwrap(),
                            );
                        }
                    }
                };
                assert_eq!(body["id"], id);
                let result = if id < 3 {
                    assert_eq!(body["method"], "tools/list");
                    assert!(!headers.to_ascii_lowercase().contains("mcp-param-fresh"));
                    json!({"resultType":"complete","tools":[{"name":"echo","inputSchema":response_schema}]})
                } else {
                    assert_eq!(body["method"], "tools/call");
                    assert_eq!(body["params"]["arguments"]["text"], "wire value");
                    assert!(headers
                        .to_ascii_lowercase()
                        .contains("mcp-param-fresh: wire value"));
                    json!({"resultType":"complete","content":[{"type":"text","text":"HTTP bridge reply"}]})
                };
                let reply = json!({"jsonrpc":"2.0","id":id,"result":result}).to_string();
                let http=format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",reply.len(),reply);
                stream.write_all(http.as_bytes()).await.unwrap();
            }
        });
        let key = [22; 32];
        let req = BridgeRequest {
            transport: Transport::Http,
            expected_schema_revision: Some(schema::revision(&input, &key)),
            call: GateRequest {
                server_id: "fixture".into(),
                tool_name: "echo".into(),
                arguments: json!({"text":"wire value"}),
                expected_revision: "a".repeat(64),
                session_id: "wire-chat".into(),
                approval_grant_id: None,
            },
        };
        let mut peer = Wire {
            url: url::Url::parse(&format!("http://{addr}/mcp")).unwrap(),
            id: 0,
        };
        let outcome = run(
            &mut peer,
            &req,
            &key,
            1,
            || Ok(()),
            |_, _| async { Ok(()) },
            true,
            &CancellationToken::new(),
        )
        .await;
        if outcome.is_err() {
            server.abort();
        }
        assert_eq!(
            outcome.unwrap().1.unwrap()["content"][0]["text"],
            "HTTP bridge reply"
        );
        server.await.unwrap();
    }
}
