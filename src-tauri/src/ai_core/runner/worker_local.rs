//! Worker-only, read-only local I/O. This is not routed from any live worker yet.
//! The trusted coordinator opens a root handle; model text supplies only a
//! relative path inside that handle, never an ambient filesystem path.
// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2024-2026 axpnet: AI-assisted (see AI-TRANSPARENCY.md)

use std::io::Read;
use std::path::{Component, Path};
use std::sync::Arc;

use cap_std::ambient_authority;
#[cfg(unix)]
use cap_std::fs::OpenOptionsExt;
use cap_std::fs::{Dir, OpenOptions};
use serde_json::{json, Value};

use super::ledger::Ledger;

const MAX_FILE_BYTES: u64 = 10_485_760;
const MAX_RESULT_BYTES: usize = 5_120;

#[derive(Clone)]
pub struct WorkerLocalRead {
    root: Arc<Dir>,
    max_result_bytes: usize,
}

impl WorkerLocalRead {
    /// Only the coordinator may choose `root`; this constructor must never
    /// accept a path supplied by a worker tool call or provider response.
    pub fn open_root(root: &Path, max_result_bytes: usize) -> Result<Self, String> {
        if max_result_bytes == 0 || max_result_bytes > MAX_RESULT_BYTES {
            return Err("Worker read result cap is invalid".into());
        }
        let root = Dir::open_ambient_dir(root, ambient_authority())
            .map_err(|e| format!("Cannot open worker root: {e}"))?;
        Ok(Self {
            root: Arc::new(root),
            max_result_bytes,
        })
    }

    fn checked_relative_path(raw: &str) -> Result<&Path, String> {
        if raw.is_empty() || raw.len() > 4096 || raw.contains('\\') || raw.contains('\0') {
            return Err("Invalid worker relative path".into());
        }
        // Reject drive/UNC syntax on every host, not only on Windows.
        if raw.contains(':') {
            return Err("Invalid worker relative path".into());
        }
        let path = Path::new(raw);
        if !path.components().all(|c| matches!(c, Component::Normal(_))) {
            return Err("Worker path must remain below the granted root".into());
        }
        Ok(path)
    }

    fn read_bounded(&self, raw: &str) -> Result<Value, String> {
        let path = Self::checked_relative_path(raw)?;
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        options.custom_flags(libc::O_NONBLOCK);
        // cap-std resolves against the open directory handle. Never replace
        // this with canonicalize + std::fs::File::open(path): that reopens an
        // attacker-controlled name after validation.
        let file = self
            .root
            .open_with(path, &options)
            .map_err(|e| format!("Worker read denied or unavailable: {e}"))?;
        let meta = file
            .metadata()
            .map_err(|e| format!("Worker file metadata failed: {e}"))?;
        if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
            return Err("Worker may read only bounded regular files".into());
        }
        let mut bytes = Vec::with_capacity(self.max_result_bytes.saturating_add(1));
        file.take((self.max_result_bytes + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|e| format!("Worker file read failed: {e}"))?;
        let truncated =
            bytes.len() > self.max_result_bytes || meta.len() > self.max_result_bytes as u64;
        bytes.truncate(self.max_result_bytes);
        Ok(json!({
            "content": String::from_utf8_lossy(&bytes),
            "size": meta.len(),
            "truncated": truncated,
        }))
    }

    /// Strict worker allowlist. The blocking closure owns the tool-step
    /// completion, so dropping the awaiting future cannot release a slot
    /// while filesystem work is still active.
    pub async fn dispatch(
        &self,
        ledger: &Ledger,
        child_id: &str,
        tool_name: &str,
        args: &Value,
    ) -> Result<Value, String> {
        let step_id = ledger.reserve_tool_step(child_id)?;
        if tool_name != "local_read" {
            ledger.finish_tool_step(&step_id)?;
            return Err("Tool is not in the worker allowlist".into());
        }
        let raw = match args.get("path").and_then(Value::as_str) {
            Some(path) => path.to_string(),
            None => {
                ledger.finish_tool_step(&step_id)?;
                return Err("local_read requires a path string".into());
            }
        };
        let scope = self.clone();
        let run = ledger.clone();
        let owner = child_id.to_string();
        let result = tokio::task::spawn_blocking(move || {
            let result = scope.read_bounded(&raw).and_then(|value| {
                let bytes = serde_json::to_vec(&value)
                    .map_err(|e| format!("Worker result serialization failed: {e}"))?;
                run.reserve_result_bytes(&owner, bytes.len() as u64)?;
                Ok(value)
            });
            let settled = run.finish_tool_step(&step_id);
            settled?;
            if run.cancellation().is_cancelled() {
                return Err("Worker run cancelled".into());
            }
            result
        })
        .await
        .map_err(|e| format!("Worker read task interrupted: {e}"))?;
        // The owning runner still rechecks immediately before chat/event
        // publication; this closes the await-to-return cancellation window.
        if ledger.cancellation().is_cancelled() {
            Err("Worker run cancelled".into())
        } else {
            result
        }
    }
}

#[cfg(test)]
mod tests;
