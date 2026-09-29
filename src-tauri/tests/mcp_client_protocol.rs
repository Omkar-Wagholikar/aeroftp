// SPDX-License-Identifier: GPL-3.0-or-later
// AA26-10A: bounded protocol/lifecycle spike, not a production MCP client.

use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

const MODERN: &str = "2026-07-28";
const LEGACY_PREFERRED: &str = "2025-11-25";
const LEGACY_AEROFTP: &str = "2024-11-05";
const MAX_FRAME: usize = 64 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

fn metadata() -> Value {
    json!({
        "io.modelcontextprotocol/protocolVersion": MODERN,
        "io.modelcontextprotocol/clientInfo": { "name": "aeroftp-fixture", "version": "1" },
        "io.modelcontextprotocol/clientCapabilities": {}
    })
}

fn modern_request(id: u32, method: &str, args: Value) -> Value {
    let mut params = args.as_object().cloned().expect("object params");
    params.insert("_meta".into(), metadata());
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

struct Peer {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    stderr_task: tokio::task::JoinHandle<usize>,
}

impl Peer {
    fn spawn(mode: &str) -> Self {
        let fixture =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mcp_stdio_fixture.mjs");
        let mut child = Command::new("node")
            .arg(fixture)
            .arg(mode)
            .kill_on_drop(true)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("Node fixture must launch");
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().expect("stdout pipe"));
        let mut stderr = child.stderr.take().expect("stderr pipe");
        // Drain without retaining diagnostics. A verbose peer cannot deadlock
        // the stdout exchange or leak its stderr into model-visible results.
        let stderr_task = tokio::spawn(async move {
            let mut chunk = [0u8; 8192];
            let mut total = 0;
            while let Ok(n) = stderr.read(&mut chunk).await {
                if n == 0 {
                    break;
                }
                total += n;
            }
            total
        });
        Self {
            child,
            stdin,
            stdout,
            stderr_task,
        }
    }

    async fn send(&mut self, message: &Value) {
        let mut bytes = serde_json::to_vec(message).expect("serialize request");
        assert!(bytes.len() < MAX_FRAME, "bounded outgoing frame");
        bytes.push(b'\n');
        self.stdin
            .as_mut()
            .expect("open stdin")
            .write_all(&bytes)
            .await
            .expect("write request");
    }

    async fn receive(&mut self) -> Result<Value, &'static str> {
        tokio::time::timeout(REQUEST_TIMEOUT, self.receive_unbounded_time())
            .await
            .map_err(|_| "timeout")?
    }

    async fn receive_unbounded_time(&mut self) -> Result<Value, &'static str> {
        let mut frame = Vec::new();
        loop {
            let available = self.stdout.fill_buf().await.map_err(|_| "read error")?;
            if available.is_empty() {
                return Err("eof");
            }
            let take = available
                .iter()
                .position(|&b| b == b'\n')
                .map_or(available.len(), |p| p + 1);
            if frame.len() + take > MAX_FRAME {
                return Err("frame too large");
            }
            let complete = available[take - 1] == b'\n';
            frame.extend_from_slice(&available[..take]);
            self.stdout.consume(take);
            if complete {
                break;
            }
        }
        let text = std::str::from_utf8(&frame).map_err(|_| "non-utf8 frame")?;
        let value: Value = serde_json::from_str(text).map_err(|_| "invalid json")?;
        if value.get("jsonrpc") != Some(&json!("2.0")) {
            return Err("invalid jsonrpc");
        }
        Ok(value)
    }

    async fn shutdown(mut self) -> (bool, usize) {
        self.stdin.take(); // Portable EOF-first shutdown.
        let graceful = tokio::time::timeout(SHUTDOWN_TIMEOUT, self.child.wait())
            .await
            .is_ok();
        if !graceful {
            self.child.kill().await.expect("kill stuck fixture");
            self.child.wait().await.expect("reap stuck fixture");
        }
        let stderr_bytes = self.stderr_task.await.expect("stderr task");
        (graceful, stderr_bytes)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Era {
    Modern,
    Legacy,
}

// Probe in a disposable sibling, including when the peer exits on discover.
// The real session starts exactly once afterwards, on a fresh process.
async fn detect_era(mode: &str) -> Result<Era, &'static str> {
    let mut probe = Peer::spawn(mode);
    probe
        .send(&modern_request(1, "server/discover", json!({})))
        .await;
    let reply = probe.receive().await;
    let era = match reply {
        Ok(value) if value["result"]["supportedVersions"].is_array() => {
            if value["result"]["supportedVersions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == MODERN)
            {
                Ok(Era::Modern)
            } else {
                Err("no supported modern version")
            }
        }
        Ok(value) if value["error"]["code"] == -32022 => {
            if value["error"]["data"]["supported"]
                .as_array()
                .is_some_and(|versions| versions.iter().any(|v| v == MODERN))
            {
                Ok(Era::Modern)
            } else {
                Err("no supported modern version")
            }
        }
        Ok(value) if value.get("result").is_some() => Err("invalid discovery result"),
        Ok(_) | Err("eof") | Err("timeout") => Ok(Era::Legacy),
        Err(other) => Err(other),
    };
    probe.shutdown().await;
    era
}

async fn legacy_handshake(session: &mut Peer) -> Result<(), &'static str> {
    session.send(&json!({ "jsonrpc": "2.0", "id": 2, "method": "initialize", "params": { "protocolVersion": LEGACY_PREFERRED, "capabilities": {}, "clientInfo": { "name": "aeroftp-fixture", "version": "1" } } })).await;
    let response = session.receive().await?;
    if response["result"]["protocolVersion"] != LEGACY_PREFERRED
        && response["result"]["protocolVersion"] != LEGACY_AEROFTP
    {
        return Err("unsupported legacy version");
    }
    session
        .send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
        .await;
    Ok(())
}

#[tokio::test]
async fn modern_discovery_and_per_request_metadata() {
    assert_eq!(detect_era("modern").await, Ok(Era::Modern));
    let mut session = Peer::spawn("modern");
    session
        .send(&modern_request(2, "tools/list", json!({})))
        .await;
    let listed = session.receive().await.unwrap();
    assert_eq!(listed["id"], 2);
    assert_eq!(listed["result"]["resultType"], "complete");
    assert_eq!(listed["result"]["tools"][0]["name"], "echo");
    session
        .send(&modern_request(
            3,
            "tools/call",
            json!({ "name": "echo", "arguments": { "text": "ok" } }),
        ))
        .await;
    let called = session.receive().await.unwrap();
    assert_eq!(called["id"], 3);
    assert_eq!(called["result"]["content"][0]["text"], "ok");
    session
        .send(&json!({ "jsonrpc": "2.0", "id": 4, "method": "tools/list", "params": {} }))
        .await;
    assert_eq!(session.receive().await.unwrap()["error"]["code"], -32602);
    assert!(session.shutdown().await.0);
}

#[tokio::test]
async fn legacy_peer_exits_on_probe_then_fresh_session_initializes() {
    assert_eq!(detect_era("legacy").await, Ok(Era::Legacy));
    let mut session = Peer::spawn("legacy");
    legacy_handshake(&mut session).await.unwrap();
    session
        .send(&json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/list" }))
        .await;
    assert_eq!(
        session.receive().await.unwrap()["result"]["tools"][0]["name"],
        "echo"
    );
    session.send(&json!({ "jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": { "name": "echo", "arguments": { "text": "legacy" } } })).await;
    assert_eq!(
        session.receive().await.unwrap()["result"]["content"][0]["text"],
        "legacy"
    );
    assert!(session.shutdown().await.0);
    assert_eq!(detect_era("legacy-new").await, Ok(Era::Legacy));
    let mut newer = Peer::spawn("legacy-new");
    legacy_handshake(&mut newer).await.unwrap();
    assert!(newer.shutdown().await.0);
}

#[tokio::test]
async fn unsupported_modern_revision_never_downgrades_to_legacy() {
    assert_eq!(
        detect_era("no-overlap").await,
        Err("no supported modern version")
    );
    let mut peer = Peer::spawn("modern");
    let mut request = modern_request(7, "server/discover", json!({}));
    request["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"] = json!("2027-01-01");
    peer.send(&request).await;
    let response = peer.receive().await.unwrap();
    assert_eq!(response["error"]["code"], -32022);
    assert_eq!(response["error"]["data"]["supported"][0], MODERN);
    peer.shutdown().await;
    assert_eq!(
        detect_era("invalid-discovery").await,
        Err("invalid discovery result")
    );
    assert_eq!(detect_era("legacy-other").await, Ok(Era::Legacy));
    let mut legacy = Peer::spawn("legacy-other");
    assert_eq!(
        legacy_handshake(&mut legacy).await,
        Err("unsupported legacy version")
    );
    legacy.shutdown().await;
}

#[tokio::test]
async fn malformed_oversized_partial_and_exit_are_bounded() {
    for (mode, expected) in [
        ("malformed", "invalid json"),
        ("oversized", "frame too large"),
        ("partial", "eof"),
        ("exit", "eof"),
    ] {
        let mut peer = Peer::spawn(mode);
        assert_eq!(peer.receive().await.unwrap_err(), expected, "{mode}");
        peer.shutdown().await;
    }
    let mut silent = Peer::spawn("silent");
    assert!(
        tokio::time::timeout(Duration::from_millis(150), silent.receive_unbounded_time())
            .await
            .is_err()
    );
    silent.shutdown().await;
}

#[tokio::test]
async fn silent_stdio_probe_falls_back_without_reusing_its_process() {
    assert_eq!(detect_era("silent").await, Ok(Era::Legacy));
}

#[tokio::test]
async fn stderr_is_drained_and_shutdown_escalates() {
    let mut noisy = Peer::spawn("stderr-flood");
    noisy
        .send(&modern_request(1, "tools/list", json!({})))
        .await;
    assert_eq!(
        noisy.receive().await.unwrap()["result"]["tools"][0]["name"],
        "echo"
    );
    let (graceful, bytes) = noisy.shutdown().await;
    assert!(graceful);
    assert_eq!(bytes, 1_000_000);
    let stubborn = Peer::spawn("stubborn");
    assert!(!stubborn.shutdown().await.0);
}

#[tokio::test]
async fn split_frame_is_reassembled_before_json_decode() {
    let mut peer = Peer::spawn("split");
    peer.send(&modern_request(1, "tools/list", json!({}))).await;
    assert_eq!(
        peer.receive().await.unwrap()["result"]["tools"][0]["name"],
        "echo"
    );
    assert!(peer.shutdown().await.0);
}

#[tokio::test]
async fn cancellation_notification_is_sent_without_result_publication() {
    let mut peer = Peer::spawn("modern");
    peer.send(&modern_request(
        8,
        "tools/call",
        json!({ "name": "wait", "arguments": {} }),
    ))
    .await;
    peer.send(&json!({ "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": 8, "reason": "test" } })).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(150), peer.receive_unbounded_time())
            .await
            .is_err()
    );
    let (graceful, stderr_bytes) = peer.shutdown().await;
    assert!(graceful);
    assert_eq!(stderr_bytes, "cancelled".len());
}
