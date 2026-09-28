use super::*;
use crate::ai_core::runner::worker_credentials::ServerPin;
use std::collections::VecDeque;
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
