//! Foreground agent loop shared by surface adapters. This is not a worker sandbox:
//! delegated authority, credentials and aggregate reservations are a later layer.
// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2024-2026 axpnet: AI-assisted (see AI-TRANSPARENCY.md)

use std::collections::HashMap;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::ai::{AIRequest, AIResponse, AIToolCall, ChatMessage, ToolCallEcho};
use ledger::{Ledger, Usage};

pub mod delegation;
pub mod ledger;
pub mod worker_coordinator;
pub mod worker_credentials;
pub mod worker_local;
pub mod worker_remote;

pub const CANCELLED: &str = "Agent run cancelled";

pub fn check_cancelled(cancel: &CancellationToken) -> Result<(), String> {
    if cancel.is_cancelled() {
        Err(CANCELLED.to_string())
    } else {
        Ok(())
    }
}

/// UI, approval, transport and accounting remain with the owning surface.
/// Transport futures must clean up on drop. Tool execution is instead awaited to
/// quiescence: dropping a future does not prove its blocking work has stopped.
#[async_trait]
pub trait RunnerAdapter: Sync {
    async fn complete(
        &self,
        request: AIRequest,
        cancel: &CancellationToken,
    ) -> Result<AIResponse, String>;
    fn account(&self, response: &AIResponse) -> Result<(), String>;
    /// Must recheck cancellation after any approval wait and before dispatch.
    /// Recoverable tool errors/denials are returned as text, fatal errors as Err.
    /// Err(CANCELLED) means no dispatch occurred. If work has already started,
    /// await it and return its real outcome even if cancelled, for the audit trail.
    async fn execute(
        &self,
        call: &AIToolCall,
        cancel: &CancellationToken,
    ) -> Result<String, String>;
    fn assistant_tools(&self, _response: &AIResponse) {}
    fn continuing(&self) {}
}

pub struct RunnerOptions {
    pub max_steps: u32,
    pub plan_only: bool,
    /// Workers fail closed when they reach the tool-step ceiling.
    pub fail_on_step_limit: bool,
}

struct ParentRequestReservation {
    ledger: Ledger,
    id: Option<String>,
}

impl ParentRequestReservation {
    fn new(ledger: &Ledger, request: &AIRequest) -> Result<Self, String> {
        // Admission is intentionally conservative: one input reservation unit
        // per serialized UTF-8 byte, not a provider tokenizer estimate. The
        // shared limit is sized as a byte ceiling; known provider tokens replace
        // this upper reservation on settlement. No guessed bytes/token divisor.
        // The model key is excluded before estimating the serialized prompt.
        // Its temporary clone is wiped rather than left in a JSON buffer.
        let mut budget_request = request.clone();
        if let Some(mut key) = budget_request.api_key.take() {
            use zeroize::Zeroize;
            key.zeroize();
        }
        let input = serde_json::to_vec(&budget_request)
            .map_err(|_| "Cannot estimate parent request budget")?
            .len() as u64;
        let output = request
            .max_tokens
            .filter(|max| *max > 0)
            .ok_or("Parent request needs an output ceiling")? as u64;
        let id = ledger.reserve_request(
            ledger.run_id(),
            Usage {
                input_tokens: input,
                output_tokens: output,
            },
        )?;
        Ok(Self {
            ledger: ledger.clone(),
            id: Some(id),
        })
    }

    fn settle(&mut self, response: &AIResponse) -> Result<(), String> {
        let actual = response
            .input_tokens
            .zip(response.output_tokens)
            .map(|(input, output)| Usage {
                input_tokens: input as u64,
                output_tokens: output as u64,
            });
        self.ledger.finish_request(
            &self.id.take().ok_or("Parent reservation already settled")?,
            actual,
        )
    }
}

impl Drop for ParentRequestReservation {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            let _ = self.ledger.finish_request(&id, None);
        }
    }
}

fn message(role: &str, content: String) -> ChatMessage {
    ChatMessage {
        native_turn: None,
        role: role.to_string(),
        content,
        images: None,
        tool_calls_echo: None,
        tool_call_id: None,
    }
}

fn claim_result(ledger: Option<&Ledger>, content: &str) -> Result<(), String> {
    if let Some(run) = ledger {
        run.reserve_result_bytes(run.run_id(), content.len() as u64)?;
    }
    Ok(())
}

/// The template pins provider/model/endpoint/tools for this invocation. Its
/// messages form the fixed prefix (CLI system prompt); supplied history is plain
/// conversation only. Native envelopes live in this stack frame, never history.
pub async fn run(
    adapter: &impl RunnerAdapter,
    template: &AIRequest,
    messages: &mut Vec<ChatMessage>,
    options: RunnerOptions,
    cancel: &CancellationToken,
) -> Result<String, String> {
    run_inner(adapter, template, messages, options, cancel, None).await
}

/// The participating parent uses the same reservation broker as its children.
/// The caller owns the ledger until every child reaches quiescence and finish.
pub async fn run_with_ledger(
    adapter: &impl RunnerAdapter,
    template: &AIRequest,
    messages: &mut Vec<ChatMessage>,
    options: RunnerOptions,
    cancel: &CancellationToken,
    ledger: &Ledger,
) -> Result<String, String> {
    let signal = cancel.clone();
    let shared = ledger.clone();
    let watcher = tokio::spawn(async move {
        signal.cancelled().await;
        let _ = shared.cancel();
    });
    let result = run_inner(adapter, template, messages, options, cancel, Some(ledger)).await;
    watcher.abort();
    if cancel.is_cancelled() {
        ledger.cancel()?;
    }
    result
}

async fn run_inner(
    adapter: &impl RunnerAdapter,
    template: &AIRequest,
    messages: &mut Vec<ChatMessage>,
    options: RunnerOptions,
    cancel: &CancellationToken,
    ledger: Option<&Ledger>,
) -> Result<String, String> {
    check_cancelled(cancel)?;
    if template
        .messages
        .iter()
        .chain(messages.iter())
        .any(|m| m.native_turn.is_some())
    {
        return Err("A new agent run cannot inherit native continuation state".to_string());
    }
    let turn_scope = uuid::Uuid::new_v4().to_string();
    let mut native_turns: HashMap<usize, crate::ai_native::NativeTurn> = HashMap::new();
    let mut steps = 0u32;

    loop {
        check_cancelled(cancel)?;
        if ledger.is_some_and(|run| run.cancellation().is_cancelled()) {
            return Err("Agent run budget or deadline exhausted".into());
        }
        let mut request = template.clone();
        request.turn_scope = Some(turn_scope.clone());
        request.tool_results = None;
        request.messages.extend_from_slice(messages);
        for (index, native) in &native_turns {
            request.messages[template.messages.len() + index].native_turn = Some(native.clone());
        }
        // The token exists before the first poll/stream registration. Dropping a
        // pending HTTP future stops transport even before response headers arrive.
        let mut reservation = ledger
            .map(|run| ParentRequestReservation::new(run, &request))
            .transpose()?;
        let response = if let Some(run) = ledger {
            let budget_cancel = run.cancellation();
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(CANCELLED.to_string()),
                _ = budget_cancel.cancelled() => return Err("Agent run budget or deadline exhausted".into()),
                response = adapter.complete(request, cancel) => response?,
            }
        } else {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(CANCELLED.to_string()),
                response = adapter.complete(request, cancel) => response?,
            }
        };
        // Known usage must survive cancellation; only publication/dispatch stops.
        let accounting = adapter.account(&response);
        let settled = reservation
            .as_mut()
            .map(|r| r.settle(&response))
            .transpose();
        check_cancelled(cancel)?;
        accounting?;
        settled?;

        let Some(calls) = response
            .tool_calls
            .as_ref()
            .filter(|calls| !calls.is_empty())
        else {
            claim_result(ledger, &response.content)?;
            return Ok(response.content);
        };
        if let Some(run) = ledger {
            let tool_bytes = serde_json::to_vec(calls)
                .map_err(|_| "Cannot bound parent tool-call result")?
                .len();
            let total = response
                .content
                .len()
                .checked_add(tool_bytes)
                .ok_or("Parent tool-call result exceeds capacity")?;
            run.reserve_result_bytes(run.run_id(), total as u64)?;
        }
        if options.plan_only {
            let lines: Vec<String> = calls
                .iter()
                .map(|call| {
                    format!(
                        "- {} {}",
                        call.name,
                        serde_json::to_string(&call.arguments).unwrap_or_else(|_| "{}".to_string())
                    )
                })
                .collect();
            let separator = if response.content.is_empty() {
                ""
            } else {
                "\n\n"
            };
            let plan = format!(
                "{}{separator}Planned tool calls:\n{}",
                response.content,
                lines.join("\n")
            );
            claim_result(ledger, &plan)?;
            return Ok(plan);
        }
        steps += 1;
        if steps > options.max_steps {
            if options.fail_on_step_limit {
                return Err("Agent tool-step limit reached".into());
            }
            if !response.content.is_empty() {
                messages.push(message("assistant", response.content.clone()));
            }
            let limit_message = format!(
                "{}\n\n[Reached max steps limit ({}).]",
                response.content, options.max_steps
            );
            claim_result(ledger, &limit_message)?;
            return Ok(limit_message);
        }

        adapter.assistant_tools(&response);
        let mut assistant = message("assistant", response.content.clone());
        assistant.tool_calls_echo = Some(
            calls
                .iter()
                .map(|call| ToolCallEcho {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: serde_json::to_string(&call.arguments).unwrap_or_default(),
                })
                .collect(),
        );
        if let Some(native) = response.native_turn {
            native_turns.insert(messages.len(), native);
        }

        // Keep completed effects in the audit history, including on cancellation.
        // Every unexecuted call receives an explicit interruption result so a
        // later user turn cannot replay an orphaned ID or silently repeat a write.
        let mut group = vec![assistant];
        let mut interrupted = None;
        let mut has_outcome = false;
        for call in calls {
            if interrupted.is_none() && cancel.is_cancelled() {
                interrupted = Some(CANCELLED.to_string());
            }
            let mut step = None;
            if interrupted.is_none() {
                if let Some(run) = ledger {
                    match run.reserve_tool_step(run.run_id()) {
                        Ok(id) => step = Some(id),
                        Err(error) => interrupted = Some(error),
                    }
                }
            }
            let content = if interrupted.is_some() {
                "Tool call not dispatched because the agent run was interrupted.".to_string()
            } else {
                let outcome = adapter.execute(call, cancel).await;
                if let (Some(run), Some(step)) = (ledger, step.as_deref()) {
                    if let Err(error) = run.finish_tool_step(step) {
                        interrupted = Some(error);
                    }
                }
                match outcome {
                    Ok(content) => {
                        has_outcome = true;
                        if let Err(error) = claim_result(ledger, &content) {
                            interrupted = Some(error);
                            "Tool completed; result withheld because the run budget was exhausted."
                                .to_string()
                        } else {
                            content
                        }
                    }
                    Err(error) => {
                        let content = if error == CANCELLED {
                            "Tool call not dispatched because the agent run was cancelled."
                        } else {
                            has_outcome = true;
                            "Tool execution interrupted; outcome unknown. Verify effects before retrying."
                        };
                        interrupted = Some(error);
                        content.to_string()
                    }
                }
            };
            let mut result = message("tool", content);
            result.tool_call_id = Some(call.id.clone());
            group.push(result);
        }
        if has_outcome {
            messages.extend(group);
        }
        check_cancelled(cancel)?;
        if let Some(error) = interrupted {
            return Err(error);
        }
        adapter.continuing();
    }
}

#[cfg(test)]
mod tests;
