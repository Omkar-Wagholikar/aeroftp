use super::*;
use crate::ai::{AIProviderType, AIToolDefinition};
use crate::ai_core::runner::ledger::{Limits, Terminal};
use crate::providers::{ProviderConfig, ProviderType};
use std::time::Duration;

struct FakeSource(Mutex<(ModelPin, ServerPin, bool, String)>);

impl WorkerCredentialSource for FakeSource {
    fn model_pin(&self, id: &str) -> Result<ModelPin, String> {
        let state = self.0.lock().unwrap();
        if state.2 || state.0.provider_id != id {
            return Err("model unavailable".into());
        }
        Ok(state.0.clone())
    }
    fn model_key(&self, id: &str) -> Result<Zeroizing<String>, String> {
        self.model_pin(id)?;
        Ok(Zeroizing::new("secret".into()))
    }
    fn server_pin(&self, id: &str) -> Result<ServerPin, String> {
        let state = self.0.lock().unwrap();
        if state.2 || state.1.profile_id != id {
            return Err("server unavailable".into());
        }
        Ok(state.1.clone())
    }
    fn server_snapshot(&self, id: &str) -> Result<PinnedServerSnapshot, String> {
        let state = self.0.lock().unwrap();
        if state.2 || state.1.profile_id != id {
            return Err("server unavailable".into());
        }
        Ok(PinnedServerSnapshot {
            pin: state.1.clone(),
            config: ProviderConfig {
                name: id.into(),
                provider_type: ProviderType::S3,
                host: state.3.clone(),
                port: None,
                username: Some("test".into()),
                password: None,
                initial_path: None,
                extra: std::collections::HashMap::from([("bucket".into(), "fixture".into())]),
            },
            secret: Zeroizing::new("server-secret".into()),
        })
    }
}

fn fixture() -> (WorkerCoordinator, Arc<FakeSource>, Ledger) {
    let ledger = Ledger::new(Limits {
        input_tokens: 100,
        output_tokens: 100,
        requests: 2,
        tool_steps: 2,
        result_bytes: 1024,
        concurrent_children: 2,
        deadline: Instant::now() + Duration::from_secs(20),
    });
    let model = ModelPin {
        provider_id: "model-id".into(),
        provider_type: "custom".into(),
        endpoint: "https://example.test/v1".into(),
        revision: "m1".into(),
    };
    let server = ServerPin {
        profile_id: "server-id".into(),
        revision: "s1".into(),
    };
    let source = Arc::new(FakeSource(Mutex::new((
        model.clone(),
        server.clone(),
        false,
        String::new(),
    ))));
    let parent = ParentScope {
        model,
        model_name: "test-model".into(),
        local_roots: BTreeMap::new(),
        remote_roots: BTreeMap::from([("server-id".into(), (server, "/safe".into()))]),
        tools: BTreeSet::new(),
    };
    let coordinator = WorkerCoordinator::new(ledger.clone(), source.clone(), parent).unwrap();
    (coordinator, source, ledger)
}

fn request(coordinator: &WorkerCoordinator) -> WorkerRequest {
    WorkerRequest {
        goal: "Inspect selected profile".into(),
        evidence: "selected facts".into(),
        model: coordinator.parent.model.clone(),
        model_name: "test-model".into(),
        local_root_ids: vec![],
        remote_profile_ids: vec!["server-id".into()],
        tools: vec![],
        expires: Instant::now() + Duration::from_secs(10),
    }
}

fn model_request() -> AIRequest {
    AIRequest {
        turn_scope: None,
        reasoning_effort: None,
        provider_type: AIProviderType::Custom,
        model: "test-model".into(),
        api_key: None,
        base_url: "https://example.test/v1".into(),
        messages: vec![],
        max_tokens: Some(10),
        temperature: None,
        tools: None,
        tool_results: None,
        thinking_budget: None,
        top_p: None,
        top_k: None,
        cached_content: None,
        web_search: None,
        use_responses_api: None,
    }
}

fn model_response(content: &str) -> AIResponse {
    AIResponse {
        native_turn: None,
        content: content.into(),
        model: "test-model".into(),
        tokens_used: Some(4),
        input_tokens: Some(2),
        output_tokens: Some(2),
        finish_reason: Some("stop".into()),
        tool_calls: None,
        cache_creation_input_tokens: None,
        cache_read_input_tokens: None,
    }
}

#[test]
fn delegated_scope_is_exact_and_bounded() {
    let (coordinator, _, ledger) = fixture();
    let mut bad = request(&coordinator);
    bad.remote_profile_ids = vec!["server".into()];
    assert!(coordinator.prepare(bad).is_err());
    let mut bad = request(&coordinator);
    bad.tools = vec!["shell".into()];
    assert!(coordinator.prepare(bad).is_err());
    let mut bad = request(&coordinator);
    bad.goal = "x".repeat(MAX_GOAL_BYTES + 1);
    assert!(coordinator.prepare(bad).is_err());
    let mut bad = request(&coordinator);
    bad.model_name = "other".into();
    assert!(coordinator.prepare(bad).is_err());
    let child = coordinator.prepare(request(&coordinator)).unwrap();
    assert_eq!(
        coordinator.resolve_remote(&child, "server-id").unwrap().1,
        "/safe"
    );
    assert!(coordinator.resolve_remote(&child, "server").is_err());
    coordinator.finish_child(&child).unwrap();
    assert!(coordinator.resolve_model(&child).is_err());
    assert_eq!(
        ledger.finish(Terminal::Completed).unwrap(),
        Terminal::Completed
    );
}

#[test]
fn profile_edit_lock_and_parent_cancel_invalidate_prepared_worker() {
    let (coordinator, source, ledger) = fixture();
    let child = coordinator.prepare(request(&coordinator)).unwrap();
    source.0.lock().unwrap().1.revision = "s2".into();
    assert!(coordinator.resolve_remote(&child, "server-id").is_err());
    source.0.lock().unwrap().2 = true;
    assert!(coordinator.resolve_model(&child).is_err());
    ledger.cancel().unwrap();
    assert!(coordinator.resolve_remote(&child, "server-id").is_err());
    coordinator.finish_child(&child).unwrap();
    assert!(coordinator.resolve_model(&child).is_err());
}

#[test]
fn remote_root_rejects_traversal_and_alias_forms() {
    for root in [
        "",
        "safe",
        "/safe/../other",
        "/safe/./other",
        "/safe\\other",
    ] {
        assert!(!valid_remote_root(root));
    }
    assert!(valid_remote_root("/safe/sub"));
}

#[tokio::test]
async fn cancelling_a_pending_s3_head_releases_the_remote_step_before_child_finish() {
    use std::sync::Arc;
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let seen = entered.clone();
    let blocked = release.clone();
    let app = axum::Router::new().fallback(axum::routing::any(
        move |_request: axum::extract::Request| {
            let seen = seen.clone();
            let blocked = blocked.clone();
            async move {
                seen.notify_one();
                blocked.notified().await;
                axum::response::Response::builder()
                    .status(200)
                    .body(axum::body::Body::empty())
                    .unwrap()
            }
        },
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });

    let (mut coordinator, source, ledger) = fixture();
    source.0.lock().unwrap().3 = endpoint;
    coordinator.parent.tools.insert("remote_stat".into());
    let mut request = request(&coordinator);
    request.tools = vec!["remote_stat".into()];
    let child = coordinator.prepare(request).unwrap();
    let coordinator = Arc::new(coordinator);
    let active = coordinator.clone();
    let task = tokio::spawn(async move {
        let result = active
            .remote_tool(&child, "server-id", "file", "remote_stat")
            .await;
        let finished = active.finish_child(&child);
        (result, finished)
    });
    tokio::time::timeout(Duration::from_secs(3), entered.notified())
        .await
        .unwrap();
    ledger.cancel().unwrap();
    let (result, finished) = tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap();
    assert!(result.is_err());
    assert!(
        finished.is_ok(),
        "remote step must quiesce before child finish: {finished:?}"
    );
    release.notify_waiters();
}

#[tokio::test]
async fn model_dispatch_rejects_parent_history_tools_and_unreserved_output() {
    let (coordinator, _, ledger) = fixture();
    let child = coordinator.prepare(request(&coordinator)).unwrap();
    let mut request = model_request();
    request.max_tokens = Some(20);
    let cap = Usage {
        input_tokens: 20,
        output_tokens: 10,
    };
    assert!(coordinator
        .complete(&child, request.clone(), cap)
        .await
        .is_err());
    let mut tools = request.clone();
    tools.max_tokens = Some(10);
    tools.tools = Some(vec![AIToolDefinition {
        name: "shell".into(),
        description: "bad".into(),
        parameters: serde_json::json!({}),
    }]);
    assert!(coordinator.complete(&child, tools, cap).await.is_err());
    let mut history = request;
    history.max_tokens = Some(10);
    history.messages.push(ChatMessage {
        native_turn: None,
        role: "user".into(),
        content: "parent secret".into(),
        images: None,
        tool_calls_echo: None,
        tool_call_id: None,
    });
    assert!(coordinator.complete(&child, history, cap).await.is_err());
    assert_eq!(ledger.snapshot().unwrap().1, 0);
    coordinator.finish_child(&child).unwrap();
}

#[tokio::test]
async fn dropped_pending_model_future_releases_slot_but_retains_unknown_charge() {
    let (coordinator, _, ledger) = fixture();
    let child = coordinator.prepare(request(&coordinator)).unwrap();
    let cap = Usage {
        input_tokens: 20,
        output_tokens: 10,
    };
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let mut pending =
        Box::pin(
            coordinator.complete_with(&child, model_request(), cap, move |_| async move {
                let _ = started_tx.send(());
                std::future::pending::<Result<AIResponse, String>>().await
            }),
        );
    tokio::select! {
        _ = &mut pending => panic!("fixture transport should remain pending"),
        _ = started_rx => {},
    }
    drop(pending);
    assert_eq!(ledger.snapshot().unwrap().0, cap);
    coordinator.finish_child(&child).unwrap();
    assert_eq!(
        ledger.finish(Terminal::Completed).unwrap(),
        Terminal::Completed
    );
}

#[tokio::test]
async fn nonempty_model_response_needs_aggregate_result_budget() {
    let (coordinator, _, ledger) = fixture();
    let child = coordinator.prepare(request(&coordinator)).unwrap();
    ledger.reserve_result_bytes(ledger.run_id(), 1024).unwrap();
    let cap = Usage {
        input_tokens: 20,
        output_tokens: 10,
    };
    let result = coordinator
        .complete_with(&child, model_request(), cap, |_| async {
            Ok(model_response("not empty"))
        })
        .await;
    assert!(result.is_err());
    assert_eq!(ledger.snapshot().unwrap().3, 1024);
    coordinator.finish_child(&child).unwrap();
}
