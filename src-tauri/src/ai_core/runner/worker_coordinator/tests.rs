use super::*;
use crate::ai::{AIProviderType, AIToolDefinition};
use crate::ai_core::runner::ledger::{Limits, Terminal};
use std::time::Duration;

struct FakeSource(Mutex<(ModelPin, ServerPin, bool)>);

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
async fn remote_profile_edit_or_lock_during_connect_is_rejected() {
    let (coordinator, source, _) = fixture();
    let child = coordinator.prepare(request(&coordinator)).unwrap();
    let records = vec![crate::ai_tools::SavedServerInfo {
        id: "server-id".into(),
        name: "alias".into(),
        host: "example.test".into(),
        port: 22,
        username: "user".into(),
        protocol: "sftp".into(),
        initial_path: None,
        provider_id: None,
    }];
    let source_edit = source.clone();
    let edited = super::super::worker_remote::connect_checked(
        &coordinator,
        &child,
        "server-id",
        &records,
        move |_| async move {
            source_edit.0.lock().unwrap().1.revision = "s2".into();
            Ok(())
        },
    )
    .await;
    assert!(edited.is_err());
    source.0.lock().unwrap().1.revision = "s1".into();
    let source_lock = source.clone();
    let locked = super::super::worker_remote::connect_checked(
        &coordinator,
        &child,
        "server-id",
        &records,
        move |_| async move {
            source_lock.0.lock().unwrap().2 = true;
            Ok(())
        },
    )
    .await;
    assert!(locked.is_err());
    coordinator.finish_child(&child).unwrap();
}

#[tokio::test]
async fn model_dispatch_rejects_parent_history_tools_and_unreserved_output() {
    let (coordinator, _, ledger) = fixture();
    let child = coordinator.prepare(request(&coordinator)).unwrap();
    let request = AIRequest {
        turn_scope: None,
        reasoning_effort: None,
        provider_type: AIProviderType::Custom,
        model: "test-model".into(),
        api_key: None,
        base_url: "https://example.test/v1".into(),
        messages: vec![],
        max_tokens: Some(20),
        temperature: None,
        tools: None,
        tool_results: None,
        thinking_budget: None,
        top_p: None,
        top_k: None,
        cached_content: None,
        web_search: None,
        use_responses_api: None,
    };
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
