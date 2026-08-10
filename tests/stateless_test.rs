//! The proxy must work against a stateless plain-JSON MCP endpoint: one that
//! answers POSTs with `content-type: application/json`, never issues an
//! `mcp-session-id`, and returns 405 for GET (no server-to-client SSE channel).

mod stateless;

use serde_json::{Value, json};
use stateless::{NotificationReply, StatelessServer};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};

const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// Drives `mcp-proxy` over raw stdio so the test sees exactly what a real MCP
/// client would see -- including the case where a request gets no reply at all.
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

    /// Reads the next message, or `None` if the proxy stayed silent.
    async fn recv(&mut self) -> anyhow::Result<Option<Value>> {
        match tokio::time::timeout(READ_TIMEOUT, self.stdout.next_line()).await {
            Ok(Ok(Some(line))) => Ok(Some(serde_json::from_str(&line)?)),
            Ok(Ok(None)) => Ok(None),
            Ok(Err(e)) => Err(e.into()),
            Err(_) => Ok(None),
        }
    }

    async fn handshake(&mut self) -> anyhow::Result<Value> {
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
        let response = self
            .recv()
            .await?
            .ok_or_else(|| anyhow::anyhow!("no initialize response"))?;
        self.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await?;
        Ok(response)
    }
}

impl Drop for ProxyHarness {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

/// The core regression: after a successful handshake, ordinary requests must
/// still be answered. Before the fix, `tools/list` got no reply at all.
async fn assert_usable_against(reply: NotificationReply) -> anyhow::Result<()> {
    let server = StatelessServer::new(reply);
    let (url, server_handle) = server.spawn().await?;

    let mut proxy = ProxyHarness::spawn(&url).await?;
    let init = proxy.handshake().await?;
    assert_eq!(
        init.pointer("/result/serverInfo/name")
            .and_then(Value::as_str),
        Some("Stateless MCP Server"),
        "unexpected initialize response: {init}"
    );

    proxy
        .send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}))
        .await?;
    let tools = proxy.recv().await?.ok_or_else(|| {
        anyhow::anyhow!("no response to tools/list against a {reply:?} stateless server")
    })?;
    assert_eq!(
        tools
            .pointer("/result/tools/0/name")
            .and_then(Value::as_str),
        Some("echo"),
        "unexpected tools/list response: {tools}"
    );

    proxy
        .send(json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {"name": "echo", "arguments": {"message": "Hello, world!"}},
        }))
        .await?;
    let called = proxy.recv().await?.ok_or_else(|| {
        anyhow::anyhow!("no response to tools/call against a {reply:?} stateless server")
    })?;
    assert_eq!(
        called
            .pointer("/result/content/0/text")
            .and_then(Value::as_str),
        Some("Hello, world!"),
        "unexpected tools/call response: {called}"
    );

    // A stateless server has no SSE channel; the proxy must never have tried.
    assert_eq!(
        server.get_count(),
        0,
        "proxy issued a GET against a server with no SSE channel"
    );
    // initialize, notifications/initialized, tools/list, tools/call -- and no
    // more. The proxy sends the notification itself, so the client's copy must
    // be absorbed rather than forwarded a second time.
    assert_eq!(
        server.post_count(),
        4,
        "unexpected number of POSTs; the initialized notification was likely duplicated"
    );

    drop(proxy);
    server_handle.abort();
    Ok(())
}

#[tokio::test]
async fn stateless_server_replying_accepted_to_notifications() -> anyhow::Result<()> {
    assert_usable_against(NotificationReply::Accepted).await
}

/// The failure that matters most: a transport that cannot carry a session must
/// fail at `initialize`, not report success and then die. A client that only
/// handshakes -- `claude mcp list`, for instance -- would otherwise show the
/// server as connected while every tool call fails.
#[tokio::test]
async fn unusable_transport_fails_at_initialize_rather_than_later() -> anyhow::Result<()> {
    let server = StatelessServer::new(NotificationReply::ServerError);
    let (url, server_handle) = server.spawn().await?;

    let mut proxy = ProxyHarness::spawn(&url).await?;
    proxy
        .send(json!({
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

    let response = proxy
        .recv()
        .await?
        .ok_or_else(|| anyhow::anyhow!("no reply to initialize at all"))?;
    assert_eq!(
        response.get("id").and_then(Value::as_i64),
        Some(1),
        "reply did not answer the initialize request: {response}"
    );
    assert!(
        response.get("error").is_some(),
        "initialize reported success against a server whose session cannot work: {response}"
    );
    assert!(
        response.get("result").is_none(),
        "initialize returned a result as well as an error: {response}"
    );

    drop(proxy);
    server_handle.abort();
    Ok(())
}

/// Some MCP clients go straight from the `initialize` response to their first
/// real request without sending `notifications/initialized`. That first request
/// must still be answered.
#[tokio::test]
async fn client_that_skips_the_initialized_notification() -> anyhow::Result<()> {
    let server = StatelessServer::new(NotificationReply::AcceptedWithBody);
    let (url, server_handle) = server.spawn().await?;

    let mut proxy = ProxyHarness::spawn(&url).await?;
    proxy
        .send(json!({
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
    proxy
        .recv()
        .await?
        .ok_or_else(|| anyhow::anyhow!("no initialize response"))?;

    // No notifications/initialized -- straight to a real request.
    proxy
        .send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}))
        .await?;
    let tools = proxy
        .recv()
        .await?
        .ok_or_else(|| anyhow::anyhow!("no response to tools/list"))?;
    assert_eq!(
        tools
            .pointer("/result/tools/0/name")
            .and_then(Value::as_str),
        Some("echo"),
        "unexpected tools/list response: {tools}"
    );

    drop(proxy);
    server_handle.abort();
    Ok(())
}

/// Tidewave 0.8.1's actual shape: 202 carrying a JSON body.
#[tokio::test]
async fn stateless_server_replying_accepted_with_body_to_notifications() -> anyhow::Result<()> {
    assert_usable_against(NotificationReply::AcceptedWithBody).await
}

#[tokio::test]
async fn stateless_server_replying_json_to_notifications() -> anyhow::Result<()> {
    assert_usable_against(NotificationReply::EmptyJsonObject).await
}

#[tokio::test]
async fn stateless_server_replying_text_plain_to_notifications() -> anyhow::Result<()> {
    assert_usable_against(NotificationReply::TextPlain).await
}

#[tokio::test]
async fn stateless_server_replying_without_content_type() -> anyhow::Result<()> {
    assert_usable_against(NotificationReply::NoContentType).await
}
