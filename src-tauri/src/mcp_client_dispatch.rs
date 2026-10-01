//! Backend-only approved STDIO call path. No Tauri command or model route.

// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2024-2026 axpnet: AI-assisted (see AI-TRANSPARENCY.md)

use std::time::Instant;

use serde_json::{Map, Value};
use tauri::AppHandle;
use tokio_util::sync::CancellationToken;

use crate::mcp_client_gate::{self, AuthorizedCall, GateError, GateRequest};
use crate::mcp_client_transport::{Limits, StdioSupervisor, TransportError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DispatchError {
    Gate(GateError),
    Transport(TransportError),
    ToolUnavailable,
}

impl From<GateError> for DispatchError {
    fn from(error: GateError) -> Self {
        Self::Gate(error)
    }
}

impl From<TransportError> for DispatchError {
    fn from(error: TransportError) -> Self {
        Self::Transport(error)
    }
}

async fn execute_authorized<F>(
    authorized: AuthorizedCall,
    mut fresh: F,
    limits: Limits,
    cancel: &CancellationToken,
) -> Result<Value, DispatchError>
where
    F: FnMut() -> Result<(), GateError>,
{
    // Every call owns its process lifetime. No grant or environment is cached.
    fresh()?;
    if cancel.is_cancelled() {
        return Err(TransportError::Cancelled.into());
    }
    let mut peer = StdioSupervisor::connect_checked(
        authorized.config,
        authorized.environment,
        limits,
        cancel,
        || fresh().map_err(DispatchError::from),
    )
    .await?;
    peer.disable_restart();
    let result = async {
        fresh()?;
        let advertised = peer.call("tools/list", Map::new(), cancel).await?;
        let schema = crate::mcp_client_schema::discover(&advertised, &authorized.tool_name)
            .map_err(|_| DispatchError::ToolUnavailable)?;
        crate::mcp_client_schema::validate_arguments(&schema, &authorized.arguments)
            .map_err(|_| DispatchError::ToolUnavailable)?;
        fresh()?;
        let mut params = Map::new();
        params.insert("name".into(), Value::String(authorized.tool_name));
        params.insert("arguments".into(), authorized.arguments);
        let result = peer.call("tools/call", params, cancel).await?;
        // A mutation during the external call cannot be undone, but its reply
        // must not escape after the active user or effective revision changes.
        fresh()?;
        Ok::<_, DispatchError>(result)
    }
    .await;
    peer.shutdown().await;
    result
}

fn audit_result(
    request: &GateRequest,
    revision_key: &[u8; 32],
    user_id: i64,
    result: &Result<Value, DispatchError>,
    started: Instant,
) {
    let status = match result {
        Ok(_) => "success",
        Err(DispatchError::Gate(GateError::StaleRevision)) => "stale",
        Err(DispatchError::Gate(_)) => "rejected",
        Err(DispatchError::Transport(TransportError::Cancelled)) => "cancelled",
        Err(DispatchError::Transport(_)) => "transport_error",
        Err(DispatchError::ToolUnavailable) => "tool_unavailable",
    };
    let tool_digest = blake3::keyed_hash(revision_key, request.tool_name.as_bytes());
    tracing::info!(
        target: "mcp_client_audit",
        user_id,
        server_id = %request.server_id,
        tool_name = %format!("h:{}", &tool_digest.to_hex()[..16]),
        status,
        duration_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64,
        "outbound MCP call result"
    );
}

/// Approval is consumed before any process may be launched. Only a future
/// backend integration can call this private entry point.
pub(crate) async fn dispatch(
    app: &AppHandle,
    revision_key: &[u8; 32],
    request: &GateRequest,
    cancel: &CancellationToken,
) -> Result<Value, DispatchError> {
    let authorized = mcp_client_gate::authorize(app, revision_key, request).await?;
    let user_id = authorized.user_id;
    let authorized_revision = authorized.environment.effective_revision.clone();
    let started = Instant::now();
    let result = execute_authorized(
        authorized,
        || {
            mcp_client_gate::revalidate_for_dispatch(
                app,
                revision_key,
                request,
                user_id,
                &authorized_revision,
            )
        },
        Limits::default(),
        cancel,
    )
    .await;
    audit_result(request, revision_key, user_id, &result, started);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::time::Duration;

    use crate::mcp_client_config::{McpServerConfig, ResolvedMcpEnvironment};

    fn fixture() -> AuthorizedCall {
        let executable = if cfg!(windows) { "node.exe" } else { "node" };
        let node = std::env::split_paths(&std::env::var_os("PATH").expect("PATH"))
            .map(|dir| dir.join(executable))
            .find(|path| path.is_file())
            .expect("Node fixture runtime");
        let script =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mcp_stdio_fixture.mjs");
        AuthorizedCall {
            config: McpServerConfig {
                id: "fixture".into(),
                command: node.to_string_lossy().into_owned(),
                args: vec![script.to_string_lossy().into_owned(), "modern".into()],
                env: BTreeMap::new(),
                enabled: true,
                revision: 1,
            },
            environment: ResolvedMcpEnvironment {
                effective_revision: "test-only".into(),
                vars: BTreeMap::new(),
            },
            user_id: 1,
            tool_name: "echo".into(),
            arguments: serde_json::json!({"text":"fixture reply"}),
        }
    }

    fn limits() -> Limits {
        Limits {
            // Match transport fixtures: loaded MSVC runners can take >3s to start Node.
            request: Duration::from_secs(10),
            shutdown: Duration::from_millis(250),
        }
    }

    #[tokio::test]
    async fn freshness_precedes_process_start() {
        let mut call = fixture();
        call.config.command = "/does-not-exist".into();
        let result = execute_authorized(
            call,
            || Err(GateError::StaleRevision),
            limits(),
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(result, Err(DispatchError::Gate(GateError::StaleRevision)));
    }

    #[tokio::test]
    async fn isolated_fixture_is_listed_called_and_rechecked() {
        let checks = Cell::new(0);
        let result = execute_authorized(
            fixture(),
            || {
                checks.set(checks.get() + 1);
                Ok(())
            },
            limits(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(result["content"][0]["text"], "fixture reply");
        assert_eq!(checks.get(), 6);
    }

    #[tokio::test]
    async fn delayed_modern_probe_keeps_metadata_and_freshness_checks() {
        let mut call = fixture();
        call.config.args[1] = "modern-slow-probe".into();
        let checks = Cell::new(0);
        let result = execute_authorized(
            call,
            || {
                checks.set(checks.get() + 1);
                Ok(())
            },
            limits(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(result["content"][0]["text"], "fixture reply");
        assert_eq!(checks.get(), 6);
    }

    #[tokio::test]
    async fn stale_revision_after_connect_suppresses_tool_call() {
        let checks = Cell::new(0);
        let result = execute_authorized(
            fixture(),
            || {
                checks.set(checks.get() + 1);
                if checks.get() == 4 {
                    Err(GateError::StaleRevision)
                } else {
                    Ok(())
                }
            },
            limits(),
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(result, Err(DispatchError::Gate(GateError::StaleRevision)));
        assert_eq!(checks.get(), 4);
    }

    #[tokio::test]
    async fn unknown_advertised_tool_is_not_called() {
        let mut call = fixture();
        call.tool_name = "absent".into();
        let result = execute_authorized(call, || Ok(()), limits(), &CancellationToken::new()).await;
        assert_eq!(result, Err(DispatchError::ToolUnavailable));
    }

    #[tokio::test]
    async fn revision_change_during_call_discards_result() {
        let checks = Cell::new(0);
        let result = execute_authorized(
            fixture(),
            || {
                checks.set(checks.get() + 1);
                if checks.get() == 6 {
                    Err(GateError::StaleRevision)
                } else {
                    Ok(())
                }
            },
            limits(),
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(result, Err(DispatchError::Gate(GateError::StaleRevision)));
        assert_eq!(checks.get(), 6);
    }

    #[tokio::test]
    #[ignore = "requires AEROFTP_MCP_SELF_TEST_BIN pointing to a built AeroFTP CLI"]
    async fn self_mcp_server_round_trip() {
        let mut call = fixture();
        call.config.command = std::env::var("AEROFTP_MCP_SELF_TEST_BIN").expect("CLI binary path");
        call.config.args = vec!["agent".into(), "--mcp".into()];
        call.tool_name = "aeroftp_mcp_info".into();
        call.arguments = serde_json::json!({});
        let result = execute_authorized(
            call,
            || Ok(()),
            Limits {
                request: Duration::from_secs(8),
                shutdown: Duration::from_secs(1),
            },
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(result.get("content").is_some());
    }
}
