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
            if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
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
                .push(b"}\n{\"jsonrpc\":\"2.0\",\"id\":2}\n")
                .unwrap(),
            vec![
                json!({"jsonrpc":"2.0","id":1}),
                json!({"jsonrpc":"2.0","id":2})
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
    fn truncated_eof_fails_closed() {
        let mut decoder = McpLineDecoder::default();
        decoder.push(b"{\"jsonrpc\":\"2.0\"").unwrap();
        assert_eq!(decoder.finish(), Err(FrameError::Incomplete));
    }
}
