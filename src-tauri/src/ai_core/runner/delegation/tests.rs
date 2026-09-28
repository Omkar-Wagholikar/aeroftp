use super::*;
use crate::ai_core::runner::worker_credentials::ServerPin;
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
    fn server_pin(&self, _id: &str) -> Result<ServerPin, String> {
        Err("No server profile".into())
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
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        run_local_delegation(
            DelegationRequest {
                provider_id: "provider-1".into(),
                model_name: "fixture-model".into(),
                root: root.path().into(),
                goal: "Run two read-only checks".into(),
            },
            source(),
            parent.clone(),
            children.clone(),
            CancellationToken::new(),
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
