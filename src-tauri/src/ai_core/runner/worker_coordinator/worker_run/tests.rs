use super::*;
use crate::ai_core::runner::ledger::{Limits, Terminal};
use crate::ai_core::runner::worker_credentials::{ModelPin, ServerPin, WorkerCredentialSource};
use crate::ai_core::runner::worker_local::WorkerLocalRead;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

struct Credentials(ModelPin);

impl WorkerCredentialSource for Credentials {
    fn model_pin(&self, id: &str) -> Result<ModelPin, String> {
        if id == self.0.provider_id {
            Ok(self.0.clone())
        } else {
            Err("Unknown model".into())
        }
    }
    fn model_key(&self, id: &str) -> Result<Zeroizing<String>, String> {
        self.model_pin(id)?;
        Ok(Zeroizing::new("fixture-key".into()))
    }
    fn server_pin(&self, _id: &str) -> Result<ServerPin, String> {
        Err("No remote profiles delegated".into())
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
            .ok_or("No scripted worker response".into())
    }
}

fn response(content: &str, calls: Option<Vec<AIToolCall>>) -> AIResponse {
    AIResponse {
        native_turn: None,
        content: content.into(),
        model: "fixture-model".into(),
        tokens_used: Some(24),
        input_tokens: Some(16),
        output_tokens: Some(8),
        finish_reason: Some("stop".into()),
        tool_calls: calls,
        cache_creation_input_tokens: None,
        cache_read_input_tokens: None,
    }
}

fn fixture() -> (
    Arc<WorkerCoordinator>,
    PreparedWorker,
    super::super::Ledger,
    tempfile::TempDir,
) {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("note.txt"), "trusted fixture text").unwrap();
    let ledger = super::super::Ledger::new(Limits {
        input_tokens: 200_000,
        output_tokens: 10_000,
        requests: 4,
        tool_steps: 4,
        result_bytes: 20_000,
        concurrent_children: 2,
        deadline: Instant::now() + Duration::from_secs(20),
    });
    let model = ModelPin {
        provider_id: "provider-1".into(),
        provider_type: "custom".into(),
        endpoint: "https://example.test/v1".into(),
        revision: "revision-1".into(),
    };
    let parent = super::super::ParentScope {
        model: model.clone(),
        model_name: "fixture-model".into(),
        local_roots: BTreeMap::from([(
            "root-1".into(),
            WorkerLocalRead::open_root(root.path(), 1024).unwrap(),
        )]),
        remote_roots: BTreeMap::new(),
        tools: BTreeSet::from(["local_read".into()]),
    };
    let coordinator = Arc::new(
        WorkerCoordinator::new(ledger.clone(), Arc::new(Credentials(model.clone())), parent)
            .unwrap(),
    );
    let child = coordinator
        .prepare(super::super::WorkerRequest {
            goal: "Summarize the selected note".into(),
            evidence: "Only root-1 is granted".into(),
            model,
            model_name: "fixture-model".into(),
            local_root_ids: vec!["root-1".into()],
            remote_profile_ids: vec![],
            tools: vec!["local_read".into()],
            expires: Instant::now() + Duration::from_secs(10),
        })
        .unwrap();
    (coordinator, child, ledger, root)
}

#[tokio::test]
async fn local_worker_reads_only_its_granted_root_and_returns_bounded_data() {
    let (coordinator, child, ledger, _root) = fixture();
    let script = Arc::new(Script {
        replies: Mutex::new(VecDeque::from([
            response(
                "",
                Some(vec![AIToolCall {
                    id: "call-1".into(),
                    name: "local_read".into(),
                    arguments: json!({"root_id":"root-1","path":"note.txt"}),
                }]),
            ),
            response("The note contains trusted fixture text.", None),
        ])),
        requests: Mutex::new(Vec::new()),
    });
    let result = coordinator
        .spawn_local_worker(child, script.clone())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.summary, "The note contains trusted fixture text.");
    let requests = script.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].turn_scope, requests[1].turn_scope);
    assert!(requests[1].messages.iter().any(|message| {
        message.role == "tool"
            && message.tool_call_id.as_deref() == Some("call-1")
            && message.content.contains("trusted fixture text")
    }));
    assert!(!format!("{:?}", requests[0]).contains("fixture-key"));
    let (_, count, steps, bytes, join) = ledger.snapshot().unwrap();
    assert_eq!((count, steps), (2, 1));
    assert!(bytes > 0);
    assert_eq!(
        join,
        crate::ai_core::runner::ledger::JoinState::Pending { child_ids: vec![] }
    );
    assert_eq!(
        ledger.finish(Terminal::Completed).unwrap(),
        Terminal::Completed
    );
}

#[tokio::test]
async fn undelegated_worker_tool_fails_and_releases_child_after_quiescence() {
    let (coordinator, child, ledger, _root) = fixture();
    let script = Arc::new(Script {
        replies: Mutex::new(VecDeque::from([response(
            "",
            Some(vec![AIToolCall {
                id: "call-shell".into(),
                name: "shell".into(),
                arguments: json!({"root_id":"root-1","path":"note.txt"}),
            }]),
        )])),
        requests: Mutex::new(Vec::new()),
    });
    assert!(coordinator
        .spawn_local_worker(child, script)
        .await
        .unwrap()
        .is_err());
    assert_eq!(
        ledger.snapshot().unwrap().4,
        crate::ai_core::runner::ledger::JoinState::Pending { child_ids: vec![] }
    );
    ledger.finish(Terminal::Failed).unwrap();
}
