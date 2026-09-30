//! Private approval and audit boundary for outbound MCP calls.
//! No Tauri command or model dispatcher invokes this module yet.

// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2024-2026 axpnet: AI-assisted (see AI-TRANSPARENCY.md)

use std::future::Future;
use std::io::{self, Write};
use std::time::Instant;

use rusqlite::Connection;
use serde::Serialize;
use serde_json::{json, Value};
use tauri::AppHandle;
use zeroize::Zeroizing;

use crate::ai_tools::{self, AiToolApprovalPreparation};
use crate::mcp_client_commands;
use crate::mcp_client_config::{McpServerConfig, ResolvedMcpEnvironment};
use crate::user_partitions;

const MAX_ARGUMENT_BYTES: usize = 64 * 1024;
const MAX_TOOL_NAME_BYTES: usize = 128;
const MAX_SESSION_ID_BYTES: usize = 128;

#[derive(Default)]
struct BoundedArguments(Vec<u8>);

impl Write for BoundedArguments {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.0.len().saturating_add(bytes.len()) > MAX_ARGUMENT_BYTES {
            return Err(io::Error::other("MCP arguments too large"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GateError {
    InvalidRequest,
    UserUnavailable,
    ConfigUnavailable,
    ConfigDisabled,
    SecretUnavailable,
    StaleRevision,
    ApprovalRequired,
}

impl GateError {
    pub(crate) const fn code(self) -> &'static str {
        match self {
            Self::InvalidRequest => "MCP_CALL_INVALID_REQUEST",
            Self::UserUnavailable => "MCP_USER_UNAVAILABLE",
            Self::ConfigUnavailable => "MCP_CONFIG_UNAVAILABLE",
            Self::ConfigDisabled => "MCP_CONFIG_DISABLED",
            Self::SecretUnavailable => "MCP_SECRET_UNAVAILABLE",
            Self::StaleRevision => "MCP_CONFIG_STALE_REVISION",
            Self::ApprovalRequired => "MCP_APPROVAL_REQUIRED",
        }
    }
}

/// Owned request fields are kept out of Debug and audit output.
pub(crate) struct GateRequest {
    pub server_id: String,
    pub tool_name: String,
    pub arguments: Value,
    pub expected_revision: String,
    pub session_id: String,
    pub approval_grant_id: Option<String>,
}

fn unsafe_display_char(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{061C}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{206F}' | '\u{FEFF}')
}

impl GateRequest {
    fn validate(&self) -> Result<Vec<u8>, GateError> {
        let server = &self.server_id;
        let valid_server = !server.is_empty()
            && server.len() <= 64
            && server.as_bytes()[0].is_ascii_alphanumeric()
            && server
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
        if !valid_server
            || self.tool_name.is_empty()
            || self.tool_name.len() > MAX_TOOL_NAME_BYTES
            || self.tool_name.chars().any(unsafe_display_char)
            || self.session_id.is_empty()
            || self.session_id.len() > MAX_SESSION_ID_BYTES
            || self.session_id.chars().any(char::is_control)
            || self.expected_revision.len() != 64
            || !self
                .expected_revision
                .bytes()
                .all(|b| b.is_ascii_hexdigit())
            || !self.arguments.is_object()
        {
            return Err(GateError::InvalidRequest);
        }
        let mut args = BoundedArguments::default();
        serde_json::to_writer(&mut args, &self.arguments).map_err(|_| GateError::InvalidRequest)?;
        Ok(args.0)
    }
}

/// Only the later, owner-gated activation slice may consume this value.
pub(crate) struct AuthorizedCall {
    pub config: McpServerConfig,
    pub environment: ResolvedMcpEnvironment,
    pub user_id: i64,
    pub tool_name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AuditStatus {
    Approved,
    Denied,
    Stale,
    Rejected,
}

impl AuditStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Approved => "approved",
            Self::Denied => "denied",
            Self::Stale => "stale",
            Self::Rejected => "rejected",
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct AuditEvent {
    pub user_id: Option<i64>,
    pub server_id: String,
    pub tool_name: String,
    pub status: AuditStatus,
    pub duration_ms: u64,
}

fn audit_event(
    request: &GateRequest,
    revision_key: &[u8; 32],
    user_id: Option<i64>,
    status: AuditStatus,
    start: Instant,
) -> AuditEvent {
    // Invalid identifiers may contain arbitrary peer text. Do not log them.
    let safe_server = request.server_id.len() <= 64
        && !request.server_id.is_empty()
        && request.server_id.as_bytes()[0].is_ascii_alphanumeric()
        && request
            .server_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    let safe_tool = !request.tool_name.is_empty()
        && request.tool_name.len() <= MAX_TOOL_NAME_BYTES
        && !request.tool_name.chars().any(unsafe_display_char);
    AuditEvent {
        user_id,
        server_id: if safe_server {
            request.server_id.clone()
        } else {
            "-".into()
        },
        tool_name: if safe_tool {
            // Tool names come from untrusted peers. A keyed digest identifies
            // repeat calls without echoing a peer-supplied secret into logs.
            format!(
                "h:{}",
                &blake3::keyed_hash(revision_key, request.tool_name.as_bytes()).to_hex()[..16]
            )
        } else {
            "-".into()
        },
        status,
        duration_ms: start.elapsed().as_millis().min(u64::MAX as u128) as u64,
    }
}

fn log_audit(event: AuditEvent) {
    tracing::info!(
        target: "mcp_client_audit",
        user_id = event.user_id.unwrap_or_default(),
        server_id = %event.server_id,
        tool_name = %event.tool_name,
        status = event.status.as_str(),
        duration_ms = event.duration_ms,
        "outbound MCP authorization"
    );
}

trait GateStore {
    fn active_user_id(&self) -> Result<i64, GateError>;
    fn catalog(&self, user_id: i64) -> Result<Vec<McpServerConfig>, GateError>;
    fn secret(&self, user_id: i64, account: &str) -> Result<Zeroizing<String>, GateError>;
}

struct SqliteStore<'a> {
    conn: &'a Connection,
    root_key: &'a [u8; 32],
}

impl GateStore for SqliteStore<'_> {
    fn active_user_id(&self) -> Result<i64, GateError> {
        user_partitions::active_user_id(self.conn)
            .map_err(|_| GateError::UserUnavailable)?
            .ok_or(GateError::UserUnavailable)
    }

    fn catalog(&self, user_id: i64) -> Result<Vec<McpServerConfig>, GateError> {
        mcp_client_commands::load(self.conn, self.root_key, user_id)
            .map_err(|_| GateError::ConfigUnavailable)
    }

    fn secret(&self, user_id: i64, account: &str) -> Result<Zeroizing<String>, GateError> {
        user_partitions::get_user_credential_for(self.conn, self.root_key, user_id, account)
            .map_err(|_| GateError::SecretUnavailable)?
            .ok_or(GateError::SecretUnavailable)
    }
}

fn preflight(
    store: &impl GateStore,
    revision_key: &[u8; 32],
    request: &GateRequest,
) -> Result<AuthorizedCall, GateError> {
    let user_id = store.active_user_id()?;
    let config = store
        .catalog(user_id)?
        .into_iter()
        .find(|config| config.id == request.server_id)
        .ok_or(GateError::ConfigUnavailable)?;
    if !config.enabled {
        return Err(GateError::ConfigDisabled);
    }
    let environment = config
        .resolve_with(revision_key, user_id, |account| {
            store.secret(user_id, account).map_err(|_| ())
        })
        .map_err(|code| match code {
            "MCP_SECRET_UNAVAILABLE" => GateError::SecretUnavailable,
            "MCP_CONFIG_DISABLED" => GateError::ConfigDisabled,
            _ => GateError::ConfigUnavailable,
        })?;
    if environment.effective_revision != request.expected_revision {
        return Err(GateError::StaleRevision);
    }
    Ok(AuthorizedCall {
        config,
        environment,
        user_id,
        tool_name: request.tool_name.clone(),
        arguments: request.arguments.clone(),
    })
}

struct ApprovalScope {
    tool_name: String,
    key: String,
}

fn approval_scope(
    request: &GateRequest,
    user_id: i64,
    revision_key: &[u8; 32],
    serialized_args: &[u8],
) -> Result<ApprovalScope, GateError> {
    let digest = blake3::keyed_hash(revision_key, serialized_args)
        .to_hex()
        .to_string();
    let key = serde_json::to_string(&json!({
        "kind": "mcp_client",
        "user_id": user_id,
        "server_id": request.server_id,
        "tool_name": request.tool_name,
        "effective_revision": request.expected_revision,
        "arguments_digest": digest,
    }))
    .map_err(|_| GateError::InvalidRequest)?;
    Ok(ApprovalScope {
        tool_name: format!("mcp:{}:{}", request.server_id, request.tool_name),
        key,
    })
}

fn status_for(result: &Result<AuthorizedCall, GateError>) -> AuditStatus {
    match result {
        Ok(_) => AuditStatus::Approved,
        Err(GateError::ApprovalRequired) => AuditStatus::Denied,
        Err(GateError::StaleRevision) => AuditStatus::Stale,
        Err(_) => AuditStatus::Rejected,
    }
}

async fn authorize_with<S, A, F, U>(
    store: &S,
    revision_key: &[u8; 32],
    request: &GateRequest,
    approve: A,
    audit: U,
) -> Result<AuthorizedCall, GateError>
where
    S: GateStore,
    A: FnOnce(ApprovalScope) -> F,
    F: Future<Output = Result<(), GateError>>,
    U: FnOnce(AuditEvent),
{
    let started = Instant::now();
    let mut audit_user = None;
    let result = async {
        let args = request.validate()?;
        let first = preflight(store, revision_key, request)?;
        audit_user = Some(first.user_id);
        let scope = approval_scope(request, first.user_id, revision_key, &args)?;
        approve(scope).await?;
        // An approval can wait while the active user, config or secret changes.
        let second = preflight(store, revision_key, request)?;
        if second.user_id != first.user_id
            || second.environment.effective_revision != first.environment.effective_revision
        {
            return Err(GateError::StaleRevision);
        }
        Ok(second)
    }
    .await;
    audit(audit_event(
        request,
        revision_key,
        audit_user,
        status_for(&result),
        started,
    ));
    result
}

/// Prepare only. This creates an approval request but cannot spawn a process.
pub(crate) async fn prepare(
    app: &AppHandle,
    revision_key: &[u8; 32],
    request: &GateRequest,
) -> Result<AiToolApprovalPreparation, GateError> {
    let args = request.validate()?;
    let (conn, root_key, _) =
        mcp_client_commands::context(app).map_err(|_| GateError::UserUnavailable)?;
    let store = SqliteStore {
        conn: &conn,
        root_key: &root_key,
    };
    let current = preflight(&store, revision_key, request)?;
    let scope = approval_scope(request, current.user_id, revision_key, &args)?;
    Ok(ai_tools::prepare_backend_approval_request(
        Some(&request.session_id),
        &scope.tool_name,
        scope.key.clone(),
        scope.key,
        false,
        format!(
            "AeroAgent wants to: Run MCP Tool\n\n  server: {}\n  tool: {}",
            request.server_id, request.tool_name
        ),
    )
    .await)
}

/// Recheck the live active-user state before and after consuming a grant.
/// The resulting value has no connection to the STDIO supervisor in this slice.
pub(crate) async fn authorize(
    app: &AppHandle,
    revision_key: &[u8; 32],
    request: &GateRequest,
) -> Result<AuthorizedCall, GateError> {
    let (conn, root_key, _) = match mcp_client_commands::context(app) {
        Ok(context) => context,
        Err(_) => {
            log_audit(audit_event(
                request,
                revision_key,
                None,
                AuditStatus::Rejected,
                Instant::now(),
            ));
            return Err(GateError::UserUnavailable);
        }
    };
    let store = SqliteStore {
        conn: &conn,
        root_key: &root_key,
    };
    let session_id = request.session_id.clone();
    let grant_id = request.approval_grant_id.clone();
    authorize_with(
        &store,
        revision_key,
        request,
        move |scope| async move {
            ai_tools::ensure_ai_tool_approval(
                Some(&session_id),
                &scope.tool_name,
                &scope.key,
                &scope.key,
                grant_id.as_deref(),
            )
            .await
            .map_err(|_| GateError::ApprovalRequired)
        },
        log_audit,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::BTreeMap;

    const KEY: [u8; 32] = [19; 32];
    const SECRET: &str = "private-fixture-secret";

    struct MockStore {
        active: Cell<i64>,
        catalogs: RefCell<BTreeMap<i64, Vec<McpServerConfig>>>,
        secrets: RefCell<BTreeMap<(i64, String), String>>,
    }

    impl MockStore {
        fn new() -> Self {
            let config = fixture_config("example");
            let account = config.vault_account("API_KEY");
            Self {
                active: Cell::new(1),
                catalogs: RefCell::new(BTreeMap::from([
                    (1, vec![config.clone()]),
                    (2, vec![config]),
                ])),
                secrets: RefCell::new(BTreeMap::from([
                    ((1, account.clone()), SECRET.into()),
                    ((2, account), SECRET.into()),
                ])),
            }
        }

        fn config(&self) -> McpServerConfig {
            self.catalogs.borrow()[&self.active.get()][0].clone()
        }

        fn revision(&self) -> String {
            let config = self.config();
            config
                .resolve_with(&KEY, self.active.get(), |account| {
                    self.secret(self.active.get(), account).map_err(|_| ())
                })
                .unwrap()
                .effective_revision
        }
    }

    impl GateStore for MockStore {
        fn active_user_id(&self) -> Result<i64, GateError> {
            Ok(self.active.get())
        }

        fn catalog(&self, user_id: i64) -> Result<Vec<McpServerConfig>, GateError> {
            self.catalogs
                .borrow()
                .get(&user_id)
                .cloned()
                .ok_or(GateError::ConfigUnavailable)
        }

        fn secret(&self, user_id: i64, account: &str) -> Result<Zeroizing<String>, GateError> {
            self.secrets
                .borrow()
                .get(&(user_id, account.to_string()))
                .cloned()
                .map(Zeroizing::new)
                .ok_or(GateError::SecretUnavailable)
        }
    }

    fn fixture_config(id: &str) -> McpServerConfig {
        let mut config = McpServerConfig {
            id: id.into(),
            command: std::env::current_exe()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            args: vec!["/fixture/server".into()],
            env: BTreeMap::new(),
            enabled: true,
            revision: 1,
        };
        config.env.insert(
            "API_KEY".into(),
            crate::mcp_client_config::McpSecretRef {
                vault_account: config.vault_account("API_KEY"),
            },
        );
        config
    }

    fn request(store: &MockStore) -> GateRequest {
        GateRequest {
            server_id: "example".into(),
            tool_name: "echo".into(),
            arguments: json!({"text":"private-argument"}),
            expected_revision: store.revision(),
            session_id: "chat-a".into(),
            approval_grant_id: None,
        }
    }

    #[tokio::test]
    async fn valid_gate_rechecks_state_and_audit_omits_secrets_and_arguments() {
        let store = MockStore::new();
        let request = request(&store);
        let events = RefCell::new(Vec::new());
        let authorized = authorize_with(
            &store,
            &KEY,
            &request,
            |scope| async move {
                assert_eq!(scope.tool_name, "mcp:example:echo");
                assert!(!scope.key.contains("private-argument"));
                assert!(!scope.key.contains(SECRET));
                Ok(())
            },
            |event| events.borrow_mut().push(event),
        )
        .await
        .unwrap();
        assert_eq!(authorized.user_id, 1);
        assert_eq!(authorized.environment.vars["API_KEY"].as_str(), SECRET);
        assert_eq!(authorized.config.id, "example");
        assert_eq!(authorized.tool_name, "echo");
        assert_eq!(authorized.arguments["text"], "private-argument");
        assert_eq!(events.borrow()[0].status, AuditStatus::Approved);
        let published = serde_json::to_string(&events.borrow()[0]).unwrap();
        for forbidden in [SECRET, "private-argument", "/fixture/server", "API_KEY"] {
            assert!(!published.contains(forbidden));
        }
    }

    #[tokio::test]
    async fn disabled_missing_secret_and_changed_revision_fail_before_approval() {
        let store = MockStore::new();
        let request = request(&store);
        store.catalogs.borrow_mut().get_mut(&1).unwrap()[0].enabled = false;
        let reject = || async { panic!("approval must not be reached") };
        assert!(matches!(
            authorize_with(&store, &KEY, &request, |_| reject(), |_| {}).await,
            Err(GateError::ConfigDisabled)
        ));
        store.catalogs.borrow_mut().get_mut(&1).unwrap()[0].enabled = true;
        store.secrets.borrow_mut().clear();
        assert!(matches!(
            authorize_with(&store, &KEY, &request, |_| reject(), |_| {}).await,
            Err(GateError::SecretUnavailable)
        ));
        store.secrets.borrow_mut().insert(
            (1, store.config().vault_account("API_KEY")),
            "rotated-secret".into(),
        );
        assert!(matches!(
            authorize_with(&store, &KEY, &request, |_| reject(), |_| {}).await,
            Err(GateError::StaleRevision)
        ));
        store.catalogs.borrow_mut().get_mut(&1).unwrap().clear();
        assert!(matches!(
            authorize_with(&store, &KEY, &request, |_| reject(), |_| {}).await,
            Err(GateError::ConfigUnavailable)
        ));
    }

    #[tokio::test]
    async fn user_secret_or_config_change_during_approval_invalidates_grant() {
        for mutation in ["user", "secret", "config"] {
            let store = MockStore::new();
            let request = request(&store);
            let events = RefCell::new(Vec::new());
            let result = authorize_with(
                &store,
                &KEY,
                &request,
                |_| async {
                    match mutation {
                        "user" => store.active.set(2),
                        "secret" => {
                            store.secrets.borrow_mut().insert(
                                (1, store.config().vault_account("API_KEY")),
                                "rotated-secret".into(),
                            );
                        }
                        "config" => {
                            store.catalogs.borrow_mut().get_mut(&1).unwrap()[0].revision += 1
                        }
                        _ => unreachable!(),
                    }
                    Ok(())
                },
                |event| events.borrow_mut().push(event),
            )
            .await;
            assert!(matches!(result, Err(GateError::StaleRevision)));
            assert_eq!(events.borrow()[0].status, AuditStatus::Stale);
        }
    }

    #[tokio::test]
    async fn scopes_separate_server_tool_arguments_user_and_revision() {
        let store = MockStore::new();
        let base_request = request(&store);
        let args = base_request.validate().unwrap();
        let baseline = approval_scope(&base_request, 1, &KEY, &args).unwrap();
        for changed in [
            GateRequest {
                server_id: "other".into(),
                ..request(&store)
            },
            GateRequest {
                tool_name: "different".into(),
                ..request(&store)
            },
            GateRequest {
                arguments: json!({"text":"other"}),
                ..request(&store)
            },
            GateRequest {
                expected_revision: "f".repeat(64),
                ..request(&store)
            },
        ] {
            let serialized = changed.validate().unwrap();
            let scope = approval_scope(&changed, 1, &KEY, &serialized).unwrap();
            assert_ne!(scope.key, baseline.key);
        }
        assert_ne!(
            approval_scope(&base_request, 2, &KEY, &args).unwrap().key,
            baseline.key
        );
        assert_eq!(baseline.tool_name, "mcp:example:echo");
    }

    #[tokio::test]
    async fn wrong_grant_is_denied_and_audited_without_raw_input() {
        let store = MockStore::new();
        let request = request(&store);
        let events = RefCell::new(Vec::new());
        let result = authorize_with(
            &store,
            &KEY,
            &request,
            |scope| async move {
                ai_tools::ensure_ai_tool_approval(
                    Some("chat-a"),
                    &scope.tool_name,
                    &scope.key,
                    &scope.key,
                    Some("wrong-grant"),
                )
                .await
                .map_err(|_| GateError::ApprovalRequired)
            },
            |event| events.borrow_mut().push(event),
        )
        .await;
        assert!(matches!(result, Err(GateError::ApprovalRequired)));
        assert_eq!(events.borrow()[0].status, AuditStatus::Denied);
        assert!(!format!("{:?}", events.borrow()[0]).contains("private-argument"));
    }

    #[tokio::test]
    async fn invalid_peer_identifiers_never_enter_audit() {
        let store = MockStore::new();
        let mut request = request(&store);
        request.server_id = "private\nserver".into();
        request.tool_name = "private\ntool".into();
        let events = RefCell::new(Vec::new());
        let result = authorize_with(
            &store,
            &KEY,
            &request,
            |_| async { panic!("approval must not be reached") },
            |event| events.borrow_mut().push(event),
        )
        .await;
        assert!(matches!(result, Err(GateError::InvalidRequest)));
        assert_eq!(events.borrow()[0].server_id, "-");
        assert_eq!(events.borrow()[0].tool_name, "-");
        assert_eq!(events.borrow()[0].status, AuditStatus::Rejected);
    }

    #[tokio::test]
    async fn invisible_tool_names_never_reach_approval_or_raw_audit() {
        for character in [
            '\u{061C}', '\u{200B}', '\u{200F}', '\u{202E}', '\u{2066}', '\u{2069}', '\u{FEFF}',
        ] {
            let store = MockStore::new();
            let mut request = request(&store);
            request.tool_name = format!("safe{character}tool");
            let events = RefCell::new(Vec::new());
            let result = authorize_with(
                &store,
                &KEY,
                &request,
                |_| async { panic!("approval must not be reached") },
                |event| events.borrow_mut().push(event),
            )
            .await;
            assert!(matches!(result, Err(GateError::InvalidRequest)));
            assert_eq!(events.borrow()[0].tool_name, "-");
            assert_eq!(events.borrow()[0].status, AuditStatus::Rejected);
        }
    }

    #[tokio::test]
    async fn oversized_arguments_are_rejected_before_approval() {
        let store = MockStore::new();
        let mut request = request(&store);
        request.arguments = json!({"data": "x".repeat(MAX_ARGUMENT_BYTES)});
        let events = RefCell::new(Vec::new());
        let result = authorize_with(
            &store,
            &KEY,
            &request,
            |_| async { panic!("approval must not be reached") },
            |event| events.borrow_mut().push(event),
        )
        .await;
        assert!(matches!(result, Err(GateError::InvalidRequest)));
        assert_eq!(events.borrow()[0].status, AuditStatus::Rejected);
    }
}
