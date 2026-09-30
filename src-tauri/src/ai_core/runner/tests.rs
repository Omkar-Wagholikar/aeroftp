use super::*;
use serde_json::json;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

fn shared_ledger(requests: u64) -> Ledger {
    Ledger::new(ledger::Limits {
        input_tokens: 50_000,
        output_tokens: 1_000,
        requests,
        tool_steps: 4,
        result_bytes: 4_096,
        concurrent_children: 2,
        deadline: Instant::now() + Duration::from_secs(30),
    })
}

#[tokio::test]
async fn participating_parent_reserves_each_continuation_and_tool_step() {
    let ledger = shared_ledger(3);
    let script = Script::with(vec![response("read", &["A"]), response("done", &[])]);
    let mut request = template();
    request.max_tokens = Some(64);
    let mut history = vec![message("user", "go".into())];
    let result = run_with_ledger(
        &script,
        &request,
        &mut history,
        RunnerOptions {
            max_steps: 2,
            plan_only: false,
            fail_on_step_limit: false,
        },
        &CancellationToken::new(),
        &ledger,
    )
    .await
    .unwrap();
    assert_eq!(result, "done");
    let (usage, requests, steps, bytes, _) = ledger.snapshot().unwrap();
    assert_eq!((requests, steps), (2, 1));
    assert_eq!(
        usage,
        Usage {
            input_tokens: 20,
            output_tokens: 4
        }
    );
    assert!(bytes >= "fixture text".len() as u64 + "done".len() as u64);
    assert_eq!(
        ledger.finish(ledger::Terminal::Completed).unwrap(),
        ledger::Terminal::Completed
    );
}

#[tokio::test]
async fn shared_request_limit_blocks_parent_continuation_without_losing_tool_audit() {
    let ledger = shared_ledger(1);
    let script = Script::with(vec![response("read", &["A"])]);
    let mut request = template();
    request.max_tokens = Some(64);
    let mut history = vec![message("user", "go".into())];
    assert!(run_with_ledger(
        &script,
        &request,
        &mut history,
        RunnerOptions {
            max_steps: 2,
            plan_only: false,
            fail_on_step_limit: false
        },
        &CancellationToken::new(),
        &ledger,
    )
    .await
    .unwrap_err()
    .contains("budget"));
    assert_eq!(script.requests.lock().unwrap().len(), 1);
    assert!(history
        .iter()
        .any(|message| message.role == "tool" && message.content == "fixture text"));
    assert_eq!(ledger.snapshot().unwrap().1, 1);
    assert_eq!(
        ledger.finish(ledger::Terminal::Failed).unwrap(),
        ledger::Terminal::BudgetExhausted
    );
}

fn template() -> AIRequest {
    serde_json::from_value(json!({
        "provider_type": "openai", "model": "gpt-6-sol", "api_key": null,
        "base_url": "https://api.openai.com/v1", "messages": [{
            "role": "system", "content": "Scripted fixture"
        }], "tools": [], "use_responses_api": true
    }))
    .unwrap()
}

fn response(content: &str, ids: &[&str]) -> AIResponse {
    serde_json::from_value(json!({
        "content": content, "model": "gpt-6-sol", "input_tokens": 10,
        "output_tokens": 2, "tokens_used": 12,
        "tool_calls": ids.iter().map(|id| json!({
            "id": id, "name": "local_read", "arguments": {"path": id}
        })).collect::<Vec<_>>()
    }))
    .unwrap()
}

#[derive(Default)]
struct Script {
    responses: Mutex<VecDeque<Result<AIResponse, String>>>,
    requests: Mutex<Vec<AIRequest>>,
    executed: Mutex<Vec<String>>,
    accounted: Mutex<u32>,
    cancel_on_response: bool,
    cancel_in_tool: bool,
    cancel_after_tool: bool,
    account_error: bool,
    fatal_tool: bool,
    tool_text: String,
}

impl Script {
    fn with(responses: Vec<AIResponse>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().map(Ok).collect()),
            tool_text: "fixture text".into(),
            ..Self::default()
        }
    }
}

#[async_trait]
impl RunnerAdapter for Script {
    async fn complete(
        &self,
        request: AIRequest,
        cancel: &CancellationToken,
    ) -> Result<AIResponse, String> {
        let mut next = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected request")?;
        // A real native envelope, validated by the existing adapter on replay.
        if next.tool_calls.as_ref().is_some_and(|c| !c.is_empty()) {
            let mut payload = vec![json!({
                "type": "reasoning", "id": "opaque-fixture", "encrypted_content": "opaque"
            })];
            payload.extend(next.tool_calls.as_ref().unwrap().iter().map(|call| {
                json!({
                    "type": "function_call", "call_id": call.id, "name": call.name,
                    "arguments": call.arguments.to_string()
                })
            }));
            next.native_turn = crate::ai_native::capture(&request, json!(payload));
        }
        crate::ai_native::validate_history(&request).map_err(|e| e.to_string())?;
        self.requests.lock().unwrap().push(request);
        if self.cancel_on_response {
            cancel.cancel();
        }
        Ok(next)
    }
    fn account(&self, _: &AIResponse) -> Result<(), String> {
        *self.accounted.lock().unwrap() += 1;
        if self.account_error {
            Err("cost limit".into())
        } else {
            Ok(())
        }
    }
    async fn execute(
        &self,
        call: &AIToolCall,
        cancel: &CancellationToken,
    ) -> Result<String, String> {
        if self.cancel_in_tool {
            cancel.cancel();
        }
        check_cancelled(cancel)?;
        self.executed.lock().unwrap().push(call.id.clone());
        if self.cancel_after_tool {
            cancel.cancel();
        }
        if self.fatal_tool {
            Err("fatal tool failure".into())
        } else {
            Ok(self.tool_text.clone())
        }
    }
}

async fn invoke(
    script: &impl RunnerAdapter,
    history: &mut Vec<ChatMessage>,
    max_steps: u32,
    plan_only: bool,
    cancel: &CancellationToken,
) -> Result<String, String> {
    run(
        script,
        &template(),
        history,
        RunnerOptions {
            max_steps,
            plan_only,
            fail_on_step_limit: false,
        },
        cancel,
    )
    .await
}

#[tokio::test]
async fn preserves_ids_native_state_usage_and_multiple_continuations() {
    let script = Script::with(vec![
        response("read", &["call-A", "call-B"]),
        response("again", &["call-C"]),
        response("done", &[]),
    ]);
    let mut history = vec![message("user", "go".into())];
    assert_eq!(
        invoke(&script, &mut history, 10, false, &CancellationToken::new())
            .await
            .unwrap(),
        "done"
    );
    let requests = script.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(requests[0].messages.iter().all(|m| m.native_turn.is_none()));
    assert!(requests
        .iter()
        .all(|r| r.turn_scope == requests[0].turn_scope));
    assert!(requests[1].messages[2].native_turn.is_some());
    assert!(requests[2].messages[5].native_turn.is_some());
    assert_eq!(
        requests[1].messages[2].tool_calls_echo.as_ref().unwrap()[1].id,
        "call-B"
    );
    assert_eq!(
        requests[1].messages[3].tool_call_id.as_deref(),
        Some("call-A")
    );
    assert_eq!(
        requests[1].messages[4].tool_call_id.as_deref(),
        Some("call-B")
    );
    assert_eq!(
        *script.executed.lock().unwrap(),
        ["call-A", "call-B", "call-C"]
    );
    assert_eq!(*script.accounted.lock().unwrap(), 3);
    assert!(history.iter().all(|m| m.native_turn.is_none()));
}

#[tokio::test]
async fn new_run_mints_scope_and_does_not_replay_old_opaque_state() {
    let script = Script::with(vec![
        response("", &["A"]),
        response("done", &[]),
        response("next", &[]),
    ]);
    let mut history = vec![];
    invoke(&script, &mut history, 2, false, &CancellationToken::new())
        .await
        .unwrap();
    invoke(&script, &mut history, 2, false, &CancellationToken::new())
        .await
        .unwrap();
    let requests = script.requests.lock().unwrap();
    assert_ne!(requests[0].turn_scope, requests[2].turn_scope);
    assert!(requests[2].messages.iter().all(|m| m.native_turn.is_none()));
}

#[tokio::test]
async fn rejects_inherited_native_state_before_dispatch() {
    let script = Script::default();
    let mut request = template();
    request.turn_scope = Some("parent".into());
    let mut history = vec![message("assistant", "parent".into())];
    history[0].native_turn = crate::ai_native::capture(&request, json!([]));
    assert!(
        invoke(&script, &mut history, 2, false, &CancellationToken::new())
            .await
            .unwrap_err()
            .contains("inherit")
    );
    assert!(script.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn plan_and_step_limit_do_not_execute_or_leave_orphan_calls() {
    let plan = Script::with(vec![response("plan", &["A"])]);
    let mut history = vec![];
    assert_eq!(
        invoke(&plan, &mut history, 0, true, &CancellationToken::new())
            .await
            .unwrap(),
        "plan\n\nPlanned tool calls:\n- local_read {\"path\":\"A\"}"
    );
    assert!(history.is_empty());
    assert!(plan.executed.lock().unwrap().is_empty());
    let limit = Script::with(vec![response("limit", &["B"])]);
    assert_eq!(
        invoke(&limit, &mut history, 0, false, &CancellationToken::new())
            .await
            .unwrap(),
        "limit\n\n[Reached max steps limit (0).]"
    );
    assert_eq!(history.len(), 1);
    assert!(history[0].tool_calls_echo.is_none());
    assert!(limit.executed.lock().unwrap().is_empty());
}

#[tokio::test]
async fn refusal_errors_and_denials_keep_existing_semantics() {
    for text in ["Error: missing file", "Tool call denied by user."] {
        let mut script = Script::with(vec![
            response("", &["A"]),
            response("I cannot proceed.", &[]),
        ]);
        script.tool_text = text.into();
        let mut history = vec![];
        assert_eq!(
            invoke(&script, &mut history, 2, false, &CancellationToken::new())
                .await
                .unwrap(),
            "I cannot proceed."
        );
        assert_eq!(history[1].content, text);
        assert_eq!(history[1].tool_call_id.as_deref(), Some("A"));
    }
    let script = Script::default();
    script
        .responses
        .lock()
        .unwrap()
        .push_back(Err("transport failure".into()));
    assert_eq!(
        invoke(&script, &mut vec![], 2, false, &CancellationToken::new())
            .await
            .unwrap_err(),
        "transport failure"
    );
}

#[tokio::test]
async fn account_and_fatal_tool_errors_do_not_publish_partial_groups() {
    for account_error in [true, false] {
        let mut script = Script::with(vec![response("", &["A", "B"])]);
        script.account_error = account_error;
        script.fatal_tool = !account_error;
        let mut history = vec![];
        assert!(
            invoke(&script, &mut history, 2, false, &CancellationToken::new())
                .await
                .is_err()
        );
        assert_eq!(history.is_empty(), account_error);
    }
}

#[tokio::test]
async fn cancel_before_registration_dispatches_nothing() {
    let script = Script::default();
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert_eq!(
        invoke(&script, &mut vec![], 2, false, &cancel)
            .await
            .unwrap_err(),
        CANCELLED
    );
    assert!(script.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn cancel_after_transport_before_publication_or_after_approval() {
    for after_response in [true, false] {
        let mut script = Script::with(vec![response("late", &["A", "B"])]);
        script.cancel_on_response = after_response;
        script.cancel_in_tool = !after_response;
        let mut history = vec![];
        assert_eq!(
            invoke(&script, &mut history, 2, false, &CancellationToken::new())
                .await
                .unwrap_err(),
            CANCELLED
        );
        assert!(history.is_empty());
        assert!(script.executed.lock().unwrap().is_empty());
        assert_eq!(*script.accounted.lock().unwrap(), 1);
    }
}

struct Pending {
    entered: Notify,
    release: Notify,
    dropped: Mutex<bool>,
    pending_tool: bool,
}

#[tokio::test]
async fn cancelled_group_keeps_completed_effect_and_marks_undispatched_ids() {
    let mut script = Script::with(vec![response("", &["A", "B"])]);
    script.cancel_after_tool = true;
    script.tool_text = "write completed: fixture effect".into();
    let mut history = vec![];
    assert_eq!(
        invoke(&script, &mut history, 2, false, &CancellationToken::new())
            .await
            .unwrap_err(),
        CANCELLED
    );
    assert_eq!(history.len(), 3);
    assert_eq!(history[1].content, "write completed: fixture effect");
    assert_eq!(history[1].tool_call_id.as_deref(), Some("A"));
    assert!(history[2].content.contains("not dispatched"));
    assert_eq!(history[2].tool_call_id.as_deref(), Some("B"));
    assert_eq!(*script.executed.lock().unwrap(), ["A"]);
}
struct MarkDrop<'a>(&'a Mutex<bool>);
impl Drop for MarkDrop<'_> {
    fn drop(&mut self) {
        *self.0.lock().unwrap() = true;
    }
}

#[async_trait]
impl RunnerAdapter for Pending {
    async fn complete(&self, _: AIRequest, _: &CancellationToken) -> Result<AIResponse, String> {
        if self.pending_tool {
            return Ok(response("", &["A", "B"]));
        }
        let _guard = MarkDrop(&self.dropped);
        self.entered.notify_one();
        std::future::pending().await
    }
    fn account(&self, _: &AIResponse) -> Result<(), String> {
        Ok(())
    }
    async fn execute(&self, _: &AIToolCall, _: &CancellationToken) -> Result<String, String> {
        let _guard = MarkDrop(&self.dropped);
        self.entered.notify_one();
        self.release.notified().await;
        Ok("late tool result".into())
    }
}

#[tokio::test]
async fn pending_http_is_dropped_but_tool_work_is_awaited_to_quiescence() {
    for pending_tool in [false, true] {
        let adapter = Arc::new(Pending {
            entered: Notify::new(),
            release: Notify::new(),
            dropped: Mutex::new(false),
            pending_tool,
        });
        let cancel = CancellationToken::new();
        let task_adapter = adapter.clone();
        let task_cancel = cancel.clone();
        let task = tokio::spawn(async move {
            let mut history = vec![];
            let result = invoke(task_adapter.as_ref(), &mut history, 2, false, &task_cancel).await;
            assert_eq!(history.is_empty(), !task_adapter.pending_tool);
            result
        });
        adapter.entered.notified().await;
        cancel.cancel();
        if pending_tool {
            tokio::task::yield_now().await;
            assert!(!task.is_finished());
            assert!(!*adapter.dropped.lock().unwrap());
            adapter.release.notify_one();
        }
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(2), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap_err(),
            CANCELLED
        );
        assert!(*adapter.dropped.lock().unwrap());
    }
}

#[test]
fn parent_conservative_admission_excludes_api_key_bytes() {
    let mut request = template();
    request.max_tokens = Some(64);
    request.api_key = None;
    request.messages = vec![message("user", "UTF-8 evidence: è界".into())];
    let without_key = shared_ledger(1);
    let first = ParentRequestReservation::new(&without_key, &request).unwrap();
    let baseline = without_key.snapshot().unwrap().0.input_tokens;
    request.api_key = Some("secret-key".repeat(10_000));
    let with_key = shared_ledger(1);
    let second = ParentRequestReservation::new(&with_key, &request).unwrap();
    assert_eq!(with_key.snapshot().unwrap().0.input_tokens, baseline);
    assert!(baseline >= request.messages[0].content.len() as u64);
    drop(first);
    drop(second);
    without_key.finish(ledger::Terminal::Completed).unwrap();
    with_key.finish(ledger::Terminal::Completed).unwrap();
}
