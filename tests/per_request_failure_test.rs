//! A request the server will not answer must be reported as that request
//! failing -- not as the connection dying.
//!
//! The proxy used to treat any failure to send as fatal: it tore the session
//! down, hid the request in a buffer, reconnected, and replayed it. The client
//! heard nothing for as long as that took, and then got an error blaming the
//! transport for what was really one bad call. Worse, unrelated calls in flight
//! at the time were swept into the buffer with it.

mod stateless;

use serde_json::{Value, json};
use stateless::{NotificationReply, StatelessServer};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};

const READ_TIMEOUT: Duration = Duration::from_secs(10);

struct ProxyHarness {
    child: Child,
    stdin: ChildStdin,
    stdout: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
}

impl ProxyHarness {
    async fn spawn(url: &str) -> anyhow::Result<Self> {
        let mut child = Command::new(env!("CARGO_BIN_EXE_mcp-proxy"))
            .arg(url)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let stdin = child.stdin.take().expect("stdin piped");
        let stdout = BufReader::new(child.stdout.take().expect("stdout piped")).lines();
        Ok(Self {
            child,
            stdin,
            stdout,
        })
    }

    async fn send(&mut self, message: Value) -> anyhow::Result<()> {
        self.stdin
            .write_all(format!("{message}\n").as_bytes())
            .await?;
        self.stdin.flush().await?;
        Ok(())
    }

    async fn recv(&mut self) -> anyhow::Result<Option<Value>> {
        match tokio::time::timeout(READ_TIMEOUT, self.stdout.next_line()).await {
            Ok(Ok(Some(line))) => Ok(Some(serde_json::from_str(&line)?)),
            Ok(Ok(None)) => Ok(None),
            Ok(Err(e)) => Err(e.into()),
            Err(_) => Ok(None),
        }
    }

    async fn handshake(&mut self) -> anyhow::Result<()> {
        self.send(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0.1.0"},
            },
        }))
        .await?;
        self.recv()
            .await?
            .ok_or_else(|| anyhow::anyhow!("no initialize response"))?;
        self.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await?;
        Ok(())
    }
}

impl Drop for ProxyHarness {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

#[tokio::test]
async fn refused_call_is_answered_and_the_session_survives() -> anyhow::Result<()> {
    let server = StatelessServer::new(NotificationReply::AcceptedWithBody).aborting_tools_call();
    let (url, server_handle) = server.spawn().await?;

    let mut proxy = ProxyHarness::spawn(&url).await?;
    proxy.handshake().await?;

    proxy
        .send(json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {"name": "echo", "arguments": {"message": "hi"}},
        }))
        .await?;

    let response = proxy
        .recv()
        .await?
        .ok_or_else(|| anyhow::anyhow!("the refused call was never answered"))?;

    assert_eq!(
        response.get("id").and_then(Value::as_i64),
        Some(2),
        "the answer did not belong to the call that failed: {response}"
    );
    let message = response
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(
        message.contains("tools/call"),
        "the error does not name the call that failed: {response}"
    );
    // The whole point of walking the source chain: the outermost reqwest
    // message is identical for every HTTP-level failure, so an error that stops
    // there tells a reader nothing about what actually went wrong.
    assert!(
        message.len() > "The server did not accept this tools/call request: ".len() + 40,
        "the error carries no underlying cause: {response}"
    );

    // The session must still be usable -- that is the difference between
    // reporting a failed call and tearing the connection down.
    proxy
        .send(json!({"jsonrpc": "2.0", "id": 3, "method": "tools/list"}))
        .await?;
    let tools = proxy
        .recv()
        .await?
        .ok_or_else(|| anyhow::anyhow!("session was torn down by one refused call"))?;
    assert_eq!(
        tools
            .pointer("/result/tools/0/name")
            .and_then(Value::as_str),
        Some("echo"),
        "unexpected tools/list response after a refused call: {tools}"
    );

    drop(proxy);
    server_handle.abort();
    Ok(())
}

/// The collateral damage the old rule caused: an unrelated request in flight
/// when another one failed used to be swept into the buffer, and only came back
/// after a full teardown, reconnect and replay.
///
/// Both calls do eventually get answered under the old rule, so the thing worth
/// asserting is that the session was never torn down at all. The server's POST
/// count says that plainly: a teardown means a second `initialize` and a second
/// `notifications/initialized` on the wire.
#[tokio::test]
async fn an_unrelated_call_in_flight_is_not_dragged_down() -> anyhow::Result<()> {
    let server = StatelessServer::new(NotificationReply::AcceptedWithBody).aborting_tools_call();
    let (url, server_handle) = server.spawn().await?;

    let mut proxy = ProxyHarness::spawn(&url).await?;
    proxy.handshake().await?;

    // Both go out before either is answered.
    proxy
        .send(json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {"name": "echo", "arguments": {"message": "hi"}},
        }))
        .await?;
    proxy
        .send(json!({"jsonrpc": "2.0", "id": 3, "method": "tools/list"}))
        .await?;

    let mut seen = std::collections::HashMap::new();
    for _ in 0..2 {
        let message = proxy
            .recv()
            .await?
            .ok_or_else(|| anyhow::anyhow!("only got {} of 2 answers", seen.len()))?;
        let id = message.get("id").and_then(Value::as_i64).unwrap_or(-1);
        seen.insert(id, message);
    }

    assert!(
        seen[&2].get("error").is_some(),
        "the failing call should have been answered with an error: {:?}",
        seen[&2]
    );
    assert_eq!(
        seen[&3]
            .pointer("/result/tools/0/name")
            .and_then(Value::as_str),
        Some("echo"),
        "the innocent call was dragged down with the failing one: {:?}",
        seen[&3]
    );
    // initialize, notifications/initialized, tools/call, tools/list. Anything
    // more means the session was torn down and re-established behind the
    // client's back.
    assert_eq!(
        server.post_count(),
        4,
        "the session was torn down and rebuilt over a single refused call"
    );

    drop(proxy);
    server_handle.abort();
    Ok(())
}
