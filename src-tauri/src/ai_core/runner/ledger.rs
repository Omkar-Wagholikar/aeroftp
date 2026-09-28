//! Shared reservation broker for a foreground parent and its bounded children.
//! No worker uses this until scoped tool I/O and credential handles are wired.
// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2024-2026 axpnet: AI-assisted (see AI-TRANSPARENCY.md)

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub requests: u64,
    pub tool_steps: u64,
    pub result_bytes: u64,
    pub concurrent_children: usize,
    pub deadline: Instant,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Terminal {
    Completed,
    Failed,
    Cancelled,
    BudgetExhausted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JoinState {
    Pending { child_ids: Vec<String> },
    Terminal(Terminal),
}

#[derive(Clone, Copy)]
struct ReservedRequest {
    input: u64,
    output: u64,
}

struct State {
    used: Usage,
    requests: u64,
    tool_steps: u64,
    result_bytes: u64,
    pending_requests: BTreeMap<String, (String, ReservedRequest)>,
    pending_steps: BTreeMap<String, String>,
    children: BTreeSet<String>,
    cancel_requested: bool,
    budget_exhausted: bool,
    terminal: Option<Terminal>,
}

#[derive(Clone)]
pub struct Ledger {
    run_id: String,
    limits: Limits,
    state: Arc<Mutex<State>>,
    notify: Arc<Notify>,
    cancel: CancellationToken,
}

impl Ledger {
    pub fn new(limits: Limits) -> Self {
        Self {
            run_id: uuid::Uuid::new_v4().to_string(),
            limits,
            state: Arc::new(Mutex::new(State {
                used: Usage::default(),
                requests: 0,
                tool_steps: 0,
                result_bytes: 0,
                pending_requests: BTreeMap::new(),
                pending_steps: BTreeMap::new(),
                children: BTreeSet::new(),
                cancel_requested: false,
                budget_exhausted: false,
                terminal: None,
            })),
            notify: Arc::new(Notify::new()),
            cancel: CancellationToken::new(),
        }
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }
    pub fn cancellation(&self) -> CancellationToken {
        self.cancel.clone()
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, State>, String> {
        self.state
            .lock()
            .map_err(|_| "Run ledger lock poisoned".to_string())
    }

    fn owner(&self, state: &State, id: &str) -> bool {
        id == self.run_id || state.children.contains(id)
    }

    fn ready(&self, state: &mut State, at: Instant) -> Result<(), String> {
        if state.terminal.is_some() {
            return Err("Run is terminal".into());
        }
        if state.cancel_requested || self.cancel.is_cancelled() {
            return Err("Run is cancelled".into());
        }
        if state.budget_exhausted {
            return Err("Run budget exhausted".into());
        }
        if at >= self.limits.deadline {
            self.exhaust(state);
            return Err("Run deadline exceeded".into());
        }
        Ok(())
    }

    fn exhaust(&self, state: &mut State) -> String {
        state.budget_exhausted = true;
        state.cancel_requested = true;
        self.cancel.cancel();
        self.notify.notify_waiters();
        "Run budget exhausted".into()
    }

    /// Reserve both token ceilings before every parent/child request, retry or
    /// continuation. Request count never refunds; unknown usage keeps the full
    /// reservation. The trusted caller supplies conservative input/output caps.
    pub fn reserve_request(&self, owner_id: &str, cap: Usage) -> Result<String, String> {
        self.reserve_request_at(owner_id, cap, Instant::now())
    }

    fn reserve_request_at(
        &self,
        owner_id: &str,
        cap: Usage,
        at: Instant,
    ) -> Result<String, String> {
        let mut state = self.lock()?;
        self.ready(&mut state, at)?;
        if !self.owner(&state, owner_id) {
            return Err("Unknown run owner".into());
        }
        if cap.input_tokens == 0 || cap.output_tokens == 0 {
            return Err("Request reservation must have positive token ceilings".into());
        }
        let fits = state
            .requests
            .checked_add(1)
            .is_some_and(|n| n <= self.limits.requests)
            && state
                .used
                .input_tokens
                .checked_add(cap.input_tokens)
                .is_some_and(|n| n <= self.limits.input_tokens)
            && state
                .used
                .output_tokens
                .checked_add(cap.output_tokens)
                .is_some_and(|n| n <= self.limits.output_tokens);
        if !fits {
            return Err(self.exhaust(&mut state));
        }
        state.requests += 1;
        state.used.input_tokens += cap.input_tokens;
        state.used.output_tokens += cap.output_tokens;
        let id = uuid::Uuid::new_v4().to_string();
        state.pending_requests.insert(
            id.clone(),
            (
                owner_id.to_string(),
                ReservedRequest {
                    input: cap.input_tokens,
                    output: cap.output_tokens,
                },
            ),
        );
        Ok(id)
    }

    /// Reconcile known provider usage. An unknown/aborted request retains its
    /// reservation. A provider exceeding its reserved ceiling exhausts the run
    /// even if the global limit still has space: later dispatch is forbidden.
    pub fn finish_request(&self, request_id: &str, usage: Option<Usage>) -> Result<(), String> {
        let mut state = self.lock()?;
        let (_, reserved) = state
            .pending_requests
            .remove(request_id)
            .ok_or("Unknown or completed request")?;
        if let Some(actual) = usage {
            state.used.input_tokens = state
                .used
                .input_tokens
                .saturating_sub(reserved.input)
                .saturating_add(actual.input_tokens);
            state.used.output_tokens = state
                .used
                .output_tokens
                .saturating_sub(reserved.output)
                .saturating_add(actual.output_tokens);
            if actual.input_tokens > reserved.input || actual.output_tokens > reserved.output {
                self.exhaust(&mut state);
                self.notify.notify_waiters();
                return Err("Provider usage exceeded request reservation".into());
            }
        }
        self.notify.notify_waiters();
        Ok(())
    }

    pub fn reserve_tool_step(&self, owner_id: &str) -> Result<String, String> {
        self.reserve_tool_step_at(owner_id, Instant::now())
    }

    fn reserve_tool_step_at(&self, owner_id: &str, at: Instant) -> Result<String, String> {
        let mut state = self.lock()?;
        self.ready(&mut state, at)?;
        if !self.owner(&state, owner_id) {
            return Err("Unknown run owner".into());
        }
        if !state
            .tool_steps
            .checked_add(1)
            .is_some_and(|n| n <= self.limits.tool_steps)
        {
            return Err(self.exhaust(&mut state));
        }
        state.tool_steps += 1;
        let id = uuid::Uuid::new_v4().to_string();
        state.pending_steps.insert(id.clone(), owner_id.to_string());
        Ok(id)
    }

    /// Call only after blocking work actually quiesces. Tool steps never refund.
    pub fn finish_tool_step(&self, step_id: &str) -> Result<(), String> {
        let mut state = self.lock()?;
        state
            .pending_steps
            .remove(step_id)
            .ok_or("Unknown or completed tool step")?;
        self.notify.notify_waiters();
        Ok(())
    }

    /// Result capacity is claimed before publication. Fetch/allocation caps
    /// remain the responsibility of the confined I/O handler.
    pub fn reserve_result_bytes(&self, owner_id: &str, bytes: u64) -> Result<(), String> {
        let mut state = self.lock()?;
        self.ready(&mut state, Instant::now())?;
        if !self.owner(&state, owner_id) {
            return Err("Unknown run owner".into());
        }
        if !state
            .result_bytes
            .checked_add(bytes)
            .is_some_and(|n| n <= self.limits.result_bytes)
        {
            return Err(self.exhaust(&mut state));
        }
        state.result_bytes += bytes;
        Ok(())
    }

    /// Only the root run can mint children. Depth is therefore exactly one.
    pub fn acquire_child(&self, parent_id: &str) -> Result<String, String> {
        let mut state = self.lock()?;
        self.ready(&mut state, Instant::now())?;
        if parent_id != self.run_id {
            return Err("Nested workers are disabled".into());
        }
        if state.children.len() >= self.limits.concurrent_children {
            return Err("Child concurrency limit".into());
        }
        let id = uuid::Uuid::new_v4().to_string();
        state.children.insert(id.clone());
        Ok(id)
    }

    /// No implicit Drop release: a tool that outlives the parent task must keep
    /// occupying its slot until actual quiescence is observed.
    pub fn finish_child(&self, child_id: &str) -> Result<(), String> {
        let mut state = self.lock()?;
        if !state.children.contains(child_id) {
            return Err("Unknown or completed child".into());
        }
        if state
            .pending_requests
            .values()
            .any(|(owner, _)| owner == child_id)
            || state.pending_steps.values().any(|owner| owner == child_id)
        {
            return Err("Child still has active work".into());
        }
        state.children.remove(child_id);
        self.notify.notify_waiters();
        Ok(())
    }

    pub fn cancel(&self) -> Result<(), String> {
        let mut state = self.lock()?;
        if state.terminal.is_some() {
            return Ok(());
        }
        state.cancel_requested = true;
        self.cancel.cancel();
        self.notify.notify_waiters();
        Ok(())
    }

    pub fn finish(&self, requested: Terminal) -> Result<Terminal, String> {
        let mut state = self.lock()?;
        if state.terminal.is_some() {
            return Err("Run already terminal".into());
        }
        if !matches!(requested, Terminal::Completed | Terminal::Failed) {
            return Err("Terminal status is derived from run state".into());
        }
        if !state.children.is_empty()
            || !state.pending_requests.is_empty()
            || !state.pending_steps.is_empty()
        {
            return Err("Run still has active work".into());
        }
        let status = if state.budget_exhausted {
            Terminal::BudgetExhausted
        } else if state.cancel_requested || self.cancel.is_cancelled() {
            Terminal::Cancelled
        } else {
            requested
        };
        state.terminal = Some(status);
        self.notify.notify_waiters();
        Ok(status)
    }

    pub fn snapshot(&self) -> Result<(Usage, u64, u64, u64, JoinState), String> {
        let state = self.lock()?;
        let join = if let Some(status) = state.terminal {
            JoinState::Terminal(status)
        } else {
            JoinState::Pending {
                child_ids: state.children.iter().cloned().collect(),
            }
        };
        Ok((
            state.used,
            state.requests,
            state.tool_steps,
            state.result_bytes,
            join,
        ))
    }

    /// A deadline can expire with children still running. Report their IDs,
    /// never fabricate a terminal state or release a concurrency slot.
    pub async fn join(&self, deadline: Instant) -> Result<JoinState, String> {
        loop {
            // Register before inspecting state so a transition between the
            // snapshot and the wait cannot be missed.
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let snapshot = self.snapshot()?.4;
            if matches!(snapshot, JoinState::Terminal(_)) || Instant::now() >= deadline {
                return Ok(snapshot);
            }
            tokio::select! {
                _ = &mut notified => {},
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => return Ok(self.snapshot()?.4),
            }
        }
    }
}

#[cfg(test)]
mod tests;
