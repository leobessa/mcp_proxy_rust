//! The proxy forwards JSON-RPC ids untouched.
//!
//! It used to rewrite every request id to a generated UUID and keep a map back
//! to the original, on the stated grounds of preventing duplicate ids. That map
//! was the only reason ids could ever be *unknown*, and an unknown id meant the
//! message was dropped with nothing but a warning -- a silent loss of a real
//! response.
//!
//! The duplicate it guarded against cannot happen. A request id is scoped to
//! the direction the request travels: the client's ids identify client requests
//! and the server's ids identify server requests, and a response is matched by
//! whichever side it arrives from. This test forces the collision and shows
//! both messages arriving correctly with the same id.

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
async fn colliding_ids_in_opposite_directions_are_unambiguous() -> anyhow::Result<()> {
    let server =
        StatelessServer::new(NotificationReply::AcceptedWithBody).colliding_ids_on_tools_call();
    let (url, server_handle) = server.spawn().await?;

    let mut proxy = ProxyHarness::spawn(&url).await?;
    proxy.handshake().await?;

    // The client's request is id 7. The server will answer with a request of
    // its own, also id 7, followed by the response to id 7.
    proxy
        .send(json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": {"name": "echo", "arguments": {"message": "hi"}},
        }))
        .await?;

    let first = proxy
        .recv()
        .await?
        .ok_or_else(|| anyhow::anyhow!("no server-initiated request forwarded"))?;
    let second = proxy
        .recv()
        .await?
        .ok_or_else(|| anyhow::anyhow!("no response to the client's own request"))?;

    // The server's request reached the client with the server's own id intact.
    assert_eq!(
        first.get("method").and_then(Value::as_str),
        Some("ping"),
        "expected the server-initiated request first: {first}"
    );
    assert_eq!(
        first.get("id").and_then(Value::as_i64),
        Some(7),
        "the server's request id was rewritten: {first}"
    );

    // The response to the client's request arrived under the client's own id,
    // and was not confused with the server's request that shares it.
    assert!(
        second.get("result").is_some(),
        "expected the response to the client's request second: {second}"
    );
    assert_eq!(
        second.get("id").and_then(Value::as_i64),
        Some(7),
        "the client's request id was rewritten: {second}"
    );

    // The client's answer to the server's request must reach the server under
    // the id the server chose -- the direction the old id map got wrong when a
    // mapping had been cleared.
    proxy
        .send(json!({"jsonrpc": "2.0", "id": 7, "result": {}}))
        .await?;
    tokio::time::sleep(Duration::from_millis(300)).await;

    let responses = server.client_responses();
    assert_eq!(
        responses.len(),
        1,
        "the client's answer never reached the server: {responses:?}"
    );
    assert_eq!(
        responses[0].get("id").and_then(Value::as_i64),
        Some(7),
        "the client's answer reached the server under the wrong id: {responses:?}"
    );

    drop(proxy);
    server_handle.abort();
    Ok(())
}
