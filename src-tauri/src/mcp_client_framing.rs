//! Bounded JSON-RPC line framing for outbound MCP STDIO peers.
//! A malformed peer terminates its session; no partial frame is exposed.

// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2024-2026 axpnet: AI-assisted (see AI-TRANSPARENCY.md)

use serde_json::Value;

pub const MAX_MCP_FRAME_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    TooLarge,
    InvalidJson,
    InvalidJsonRpc,
    Incomplete,
}

#[derive(Default)]
pub struct McpLineDecoder {
    pending: Vec<u8>,
}

impl McpLineDecoder {
    /// A session must be discarded after any error; the decoder deliberately
    /// does not attempt to resynchronize on an untrusted peer's stdout.
    pub fn push(&mut self, mut chunk: &[u8]) -> Result<Vec<Value>, FrameError> {
        let mut messages = Vec::new();
        while !chunk.is_empty() {
            let take = chunk
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(chunk.len(), |n| n + 1);
            if self.pending.len().saturating_add(take) > MAX_MCP_FRAME_BYTES {
                self.pending.clear();
                return Err(FrameError::TooLarge);
            }
            self.pending.extend_from_slice(&chunk[..take]);
            chunk = &chunk[take..];
            if self.pending.last() != Some(&b'\n') {
                continue;
            }
            let message: Value =
                serde_json::from_slice(&self.pending).map_err(|_| FrameError::InvalidJson)?;
            self.pending.clear();
            let Some(object) = message.as_object() else {
                return Err(FrameError::InvalidJsonRpc);
            };
            if !valid_message(object) {
                return Err(FrameError::InvalidJsonRpc);
            }
            messages.push(message);
        }
        Ok(messages)
    }

    pub fn finish(self) -> Result<(), FrameError> {
        if self.pending.is_empty() {
            Ok(())
        } else {
            Err(FrameError::Incomplete)
        }
    }
}

/// The frame size bounds every field; reject ambiguous envelopes before dispatch.
fn valid_message(object: &serde_json::Map<String, Value>) -> bool {
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return false;
    }
    let valid_id = |id: &Value| id.is_null() || id.is_string() || id.is_number();
    if object.get("id").is_some_and(|id| !valid_id(id)) {
        return false;
    }
    if let Some(method) = object.get("method") {
        return method.is_string()
            && !object.contains_key("result")
            && !object.contains_key("error")
            && object
                .get("params")
                .is_none_or(|params| params.is_object() || params.is_array());
    }
    if !object.contains_key("id") || object.contains_key("result") == object.contains_key("error") {
        return false;
    }
    object.get("error").is_none_or(|error| {
        error.as_object().is_some_and(|error| {
            error
                .get("code")
                .is_some_and(|code| code.is_i64() || code.is_u64())
                && error.get("message").is_some_and(Value::is_string)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fragmented_and_multiple_frames() {
        let mut decoder = McpLineDecoder::default();
        assert!(decoder
            .push(b"{\"jsonrpc\":\"2.0\",\"id\":1")
            .unwrap()
            .is_empty());
        assert_eq!(
            decoder
                .push(b",\"result\":null}\n{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{}}\n")
                .unwrap(),
            vec![
                json!({"jsonrpc":"2.0","id":1,"result":null}),
                json!({"jsonrpc":"2.0","id":2,"result":{}})
            ]
        );
        assert_eq!(decoder.finish(), Ok(()));
    }

    #[test]
    fn oversized_and_malformed_frames_fail_closed() {
        let mut decoder = McpLineDecoder::default();
        assert_eq!(decoder.push(&vec![b'a'; MAX_MCP_FRAME_BYTES]), Ok(vec![]));
        assert_eq!(decoder.push(b"\n"), Err(FrameError::TooLarge));
        assert_eq!(
            McpLineDecoder::default().push(b"not json\n"),
            Err(FrameError::InvalidJson)
        );
        assert_eq!(
            McpLineDecoder::default().push(b"{}\n"),
            Err(FrameError::InvalidJsonRpc)
        );
    }

    #[test]
    fn invalid_envelopes_never_reach_dispatch() {
        for message in [
            json!({"jsonrpc":"2.0"}),
            json!({"jsonrpc":"2.0","id":1}),
            json!({"jsonrpc":"2.0","result":null}),
            json!({"jsonrpc":"2.0","id":1,"result":null,"error":{"code":1,"message":"error"}}),
            json!({"jsonrpc":"2.0","method":42}),
            json!({"jsonrpc":"2.0","method":"tools/list","params":null}),
            json!({"jsonrpc":"2.0","method":"tools/list","id":true}),
            json!({"jsonrpc":"2.0","method":"tools/list","result":null}),
            json!({"jsonrpc":"2.0","id":{},"result":null}),
            json!({"jsonrpc":"2.0","id":1,"error":null}),
            json!({"jsonrpc":"2.0","id":1,"error":{"code":1.5,"message":"error"}}),
            json!({"jsonrpc":"2.0","id":1,"error":{"code":1,"message":42}}),
        ] {
            let mut bytes = serde_json::to_vec(&message).unwrap();
            bytes.push(b'\n');
            assert_eq!(
                McpLineDecoder::default().push(&bytes),
                Err(FrameError::InvalidJsonRpc),
                "{message}"
            );
        }
    }

    #[test]
    fn valid_requests_notifications_and_error_responses_are_preserved() {
        for message in [
            json!({"jsonrpc":"2.0","method":"notifications/tools/list_changed"}),
            json!({"jsonrpc":"2.0","method":"tools/list","params":{},"id":"request"}),
            json!({"jsonrpc":"2.0","method":"tools/list","params":[],"id":null}),
            json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"parse error","data":{}}}),
        ] {
            let mut bytes = serde_json::to_vec(&message).unwrap();
            bytes.push(b'\n');
            assert_eq!(McpLineDecoder::default().push(&bytes), Ok(vec![message]));
        }
    }

    #[test]
    fn truncated_eof_fails_closed() {
        let mut decoder = McpLineDecoder::default();
        decoder.push(b"{\"jsonrpc\":\"2.0\"").unwrap();
        assert_eq!(decoder.finish(), Err(FrameError::Incomplete));
    }
}
