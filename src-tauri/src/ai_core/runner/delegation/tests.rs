use super::*;
use crate::ai_core::runner::worker_credentials::{PinnedServerSnapshot, ServerPin};
use crate::providers::{ProviderConfig, ProviderType};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use zeroize::Zeroizing;

struct Credentials(ModelPin);

impl WorkerCredentialSource for Credentials {
    fn model_pin(&self, id: &str) -> Result<ModelPin, String> {
        if id == self.0.provider_id {
            Ok(self.0.clone())
        } else {
            Err("Unknown provider".into())
        }
    }
    fn model_key(&self, id: &str) -> Result<Zeroizing<String>, String> {
        self.model_pin(id)?;
        Ok(Zeroizing::new("fixture-secret".into()))
    }
    fn server_pin(&self, id: &str) -> Result<ServerPin, String> {
        if !matches!(id, "server-one" | "server-two") {
            return Err("No server profile".into());
        }
        Ok(ServerPin {
            profile_id: id.into(),
            revision: format!("pin-{id}"),
        })
    }
    fn server_snapshot(&self, id: &str) -> Result<PinnedServerSnapshot, String> {
        Ok(PinnedServerSnapshot {
            pin: self.server_pin(id)?,
            config: ProviderConfig {
                name: id.into(),
                provider_type: ProviderType::S3,
                host: "s3.example.test".into(),
                port: None,
                username: Some(id.into()),
                password: None,
                initial_path: Some(if id == "server-one" { "/one" } else { "/two" }.into()),
                extra: std::collections::HashMap::from([("bucket".into(), id.into())]),
            },
            secret: Zeroizing::new(format!("key-{id}")),
        })
    }
}

struct Script {
    replies: Mutex<VecDeque<AIResponse>>,
    requests: Mutex<Vec<AIRequest>>,
}

#[async_trait]
impl WorkerTransport for Script {
    async fn complete(&self, request: AIRequest) -> Result<AIResponse, String> {
        self.requests.lock().unwrap().push(request);
        self.replies
            .lock()
            .unwrap()
            .pop_front()
            .ok_or("No scripted response".into())
    }
}

struct ConcurrentChildren {
    barrier: tokio::sync::Barrier,
    active: AtomicUsize,
    peak: AtomicUsize,
}

#[async_trait]
impl WorkerTransport for ConcurrentChildren {
    async fn complete(&self, request: AIRequest) -> Result<AIResponse, String> {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(active, Ordering::SeqCst);
        self.barrier.wait().await;
        self.active.fetch_sub(1, Ordering::SeqCst);
        let goal = request.messages[1]
            .content
            .lines()
            .nth(1)
            .unwrap_or("unknown");
        Ok(response(&format!("Summary for {goal}"), None))
    }
}

fn response(content: &str, call: Option<(&str, &str, Value)>) -> AIResponse {
    AIResponse {
        native_turn: None,
        content: content.into(),
        model: "fixture-model".into(),
        tokens_used: Some(24),
        input_tokens: Some(16),
        output_tokens: Some(8),
        finish_reason: Some("stop".into()),
        tool_calls: call.map(|(id, name, arguments)| {
            vec![AIToolCall {
                id: id.into(),
                name: name.into(),
                arguments,
            }]
        }),
        cache_creation_input_tokens: None,
        cache_read_input_tokens: None,
    }
}

fn source() -> Arc<dyn WorkerCredentialSource> {
    Arc::new(Credentials(ModelPin {
        provider_id: "provider-1".into(),
        provider_type: "custom".into(),
        endpoint: "https://example.test/v1".into(),
        revision: "fixture-revision".into(),
    }))
}

#[tokio::test]
async fn parent_and_worker_share_one_run_and_publish_only_bounded_summary() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("note.txt"), "file fixture evidence").unwrap();
    let parent = Arc::new(Script {
        replies: Mutex::new(VecDeque::from([
            response(
                "",
                Some((
                    "delegate-1",
                    "delegate_local_read",
                    json!({"root_id":"workspace","goal":"Read note.txt","evidence":"selected evidence"}),
                )),
            ),
            response("Parent summary from worker evidence.", None),
        ])),
        requests: Mutex::new(Vec::new()),
    });
    let child = Arc::new(Script {
        replies: Mutex::new(VecDeque::from([
            response(
                "",
                Some((
                    "read-1",
                    "local_read",
                    json!({"root_id":"workspace","path":"note.txt"}),
                )),
            ),
            response("The note contains file fixture evidence.", None),
        ])),
        requests: Mutex::new(Vec::new()),
    });
    let result = run_local_delegation(
        DelegationRequest {
            provider_id: "provider-1".into(),
            model_name: "fixture-model".into(),
            root: root.path().into(),
            goal: "Summarize note.txt through a worker".into(),
            remote_profiles: vec![],
        },
        source(),
        parent.clone(),
        child.clone(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(result.answer, "Parent summary from worker evidence.");
    assert_eq!(result.workers.len(), 1);
    assert_eq!(result.terminal, Terminal::Completed);
    assert_eq!(parent.requests.lock().unwrap().len(), 2);
    assert_eq!(child.requests.lock().unwrap().len(), 2);
    assert!(parent.requests.lock().unwrap()[1]
        .messages
        .iter()
        .any(|message| {
            message.role == "tool" && message.content.contains("The note contains")
        }));
    assert!(child.requests.lock().unwrap()[1]
        .messages
        .iter()
        .any(|message| {
            message.role == "tool" && message.content.contains("file fixture evidence")
        }));
    assert!(result.usage.input_tokens > 0);
}

#[tokio::test]
async fn delegated_run_denies_ungranted_parent_tool_before_child_dispatch() {
    let root = tempfile::tempdir().unwrap();
    let parent = Arc::new(Script {
        replies: Mutex::new(VecDeque::from([response(
            "",
            Some((
                "bad-1",
                "shell",
                json!({"root_id":"workspace","goal":"write a file"}),
            )),
        )])),
        requests: Mutex::new(Vec::new()),
    });
    let child = Arc::new(Script {
        replies: Mutex::new(VecDeque::new()),
        requests: Mutex::new(Vec::new()),
    });
    let result = run_local_delegation(
        DelegationRequest {
            provider_id: "provider-1".into(),
            model_name: "fixture-model".into(),
            root: root.path().into(),
            goal: "Read-only task".into(),
            remote_profiles: vec![],
        },
        source(),
        parent,
        child.clone(),
        CancellationToken::new(),
    )
    .await;
    assert!(result.is_err());
    assert!(child.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn two_children_run_concurrently_and_return_distinct_results() {
    let root = tempfile::tempdir().unwrap();
    let parent = Arc::new(Script {
        replies: Mutex::new(VecDeque::from([
            response(
                "",
                Some((
                    "delegate-2",
                    "delegate_local_reads",
                    json!({"tasks":[
                        {"root_id":"workspace","goal":"first"},
                        {"root_id":"workspace","goal":"second"}
                    ]}),
                )),
            ),
            response("Combined summary.", None),
        ])),
        requests: Mutex::new(Vec::new()),
    });
    let children = Arc::new(ConcurrentChildren {
        barrier: tokio::sync::Barrier::new(2),
        active: AtomicUsize::new(0),
        peak: AtomicUsize::new(0),
    });
    let events = Arc::new(Mutex::new(Vec::<DelegationEvent>::new()));
    let recorded = events.clone();
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        run_local_delegation_with_events(
            DelegationRequest {
                provider_id: "provider-1".into(),
                model_name: "fixture-model".into(),
                root: root.path().into(),
                goal: "Run two read-only checks".into(),
                remote_profiles: vec![],
            },
            source(),
            parent.clone(),
            children.clone(),
            CancellationToken::new(),
            Some(Arc::new(move |event| recorded.lock().unwrap().push(event))),
        ),
    )
    .await
    .expect("both workers should reach the barrier")
    .unwrap();
    assert_eq!(children.peak.load(Ordering::SeqCst), 2);
    assert_eq!(children.active.load(Ordering::SeqCst), 0);
    assert_eq!(result.workers.len(), 2);
    assert_ne!(result.workers[0].child_id, result.workers[1].child_id);
    assert_eq!(result.workers[0].run_id, result.workers[1].run_id);
    assert_eq!(result.workers[0].summary, "Summary for first");
    assert_eq!(result.workers[1].summary, "Summary for second");
    let continuation = &parent.requests.lock().unwrap()[1];
    assert!(continuation.messages.iter().any(|message| {
        message.role == "tool"
            && message.content.contains("Summary for first")
            && message.content.contains("Summary for second")
    }));
    let events = events.lock().unwrap();
    assert_eq!(events.first().unwrap().status, "running");
    assert_eq!(events.last().unwrap().status, "completed");
    assert_eq!(
        events
            .iter()
            .filter(|event| event.status == "completed" && event.child_id.is_some())
            .count(),
        2
    );
    assert!(events
        .iter()
        .enumerate()
        .all(|(index, event)| event.sequence == index as u64 + 1));
    let payload = serde_json::to_string(&*events).unwrap();
    assert!(!payload.contains("fixture-secret"));
    assert!(!payload.contains("Summary for"));
    let root_path = root.path().to_string_lossy().into_owned();
    assert!(!payload.contains(root_path.as_str()));
}

#[tokio::test]
async fn two_remote_profiles_keep_distinct_child_identities_and_one_budget() {
    let root = tempfile::tempdir().unwrap();
    let parent = Arc::new(Script {
        replies: Mutex::new(VecDeque::from([
            response(
                "",
                Some((
                    "remote-2",
                    "delegate_remote_reads",
                    json!({"tasks": [
                        {"profile_id": "server-one", "goal": "first"},
                        {"profile_id": "server-two", "goal": "second"}
                    ]}),
                )),
            ),
            response("Both profiles checked.", None),
        ])),
        requests: Mutex::new(Vec::new()),
    });
    let children = Arc::new(ConcurrentChildren {
        barrier: tokio::sync::Barrier::new(2),
        active: AtomicUsize::new(0),
        peak: AtomicUsize::new(0),
    });
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        run_local_delegation(
            DelegationRequest {
                provider_id: "provider-1".into(),
                model_name: "fixture-model".into(),
                root: root.path().into(),
                goal: "Compare two selected profiles".into(),
                remote_profiles: vec![
                    RemoteProfileScope {
                        profile_id: "server-one".into(),
                        root: "/one".into(),
                    },
                    RemoteProfileScope {
                        profile_id: "server-two".into(),
                        root: "/two".into(),
                    },
                ],
            },
            source(),
            parent.clone(),
            children.clone(),
            CancellationToken::new(),
        ),
    )
    .await
    .expect("both remote workers should reach the barrier")
    .unwrap();
    assert_eq!(children.peak.load(Ordering::SeqCst), 2);
    assert_eq!(
        result
            .workers
            .iter()
            .map(|worker| worker.profile_id.as_deref())
            .collect::<Vec<_>>(),
        vec![Some("server-one"), Some("server-two")]
    );
    let continuation = &parent.requests.lock().unwrap()[1];
    let tool_result = continuation
        .messages
        .iter()
        .find(|message| message.role == "tool")
        .unwrap();
    assert!(tool_result.content.contains("server-one"));
    assert!(tool_result.content.contains("server-two"));
    assert!(!tool_result.content.contains("key-server"));
}

#[tokio::test]
async fn invalid_second_task_releases_prepared_first_child_without_dispatch() {
    let root = tempfile::tempdir().unwrap();
    let parent = Arc::new(Script {
        replies: Mutex::new(VecDeque::from([response(
            "",
            Some((
                "delegate-invalid",
                "delegate_local_reads",
                json!({"tasks":[
                    {"root_id":"workspace","goal":"first"},
                    {"root_id":"outside","goal":"second"}
                ]}),
            )),
        )])),
        requests: Mutex::new(Vec::new()),
    });
    let child = Arc::new(Script {
        replies: Mutex::new(VecDeque::new()),
        requests: Mutex::new(Vec::new()),
    });
    let error = run_local_delegation(
        DelegationRequest {
            provider_id: "provider-1".into(),
            model_name: "fixture-model".into(),
            root: root.path().into(),
            goal: "Try a batch".into(),
            remote_profiles: vec![],
        },
        source(),
        parent,
        child.clone(),
        CancellationToken::new(),
    )
    .await
    .err()
    .expect("invalid second task must fail");
    assert!(error.contains("granted scope"), "{error}");
    assert!(child.requests.lock().unwrap().is_empty());
}
