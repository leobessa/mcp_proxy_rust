//! A stateless, plain-JSON JSON-RPC endpoint shaped like Tidewave 0.8.1.
//!
//! The whole surface is: POST returns `content-type: application/json` with no
//! `mcp-session-id` header, and GET returns 405 (no server-to-client channel).

// Shared by several test binaries; each uses only part of it.
#![allow(dead_code)]

use axum::{
    Router,
    body::Body,
    extract::State,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::any,
};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

/// How the endpoint answers a POST that carries a JSON-RPC *notification*
/// (no `id`, so JSON-RPC forbids a response payload). Real servers disagree
/// wildly here, and the proxy has to survive all of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationReply {
    /// 202 with an empty body. The Streamable HTTP spec's recommendation.
    Accepted,
    /// 202 with `content-type: application/json` and a `{"status":"ok"}` body.
    /// This is what Tidewave 0.8.1 actually sends.
    AcceptedWithBody,
    /// 200 with `content-type: application/json` and a `{}` body.
    EmptyJsonObject,
    /// 200 with `content-type: text/plain` and an empty body.
    TextPlain,
    /// 200 with a body but no `content-type` header at all.
    NoContentType,
    /// 500 with an opaque body. Nothing here is recoverable: the server
    /// accepted `initialize` but cannot carry a session.
    ServerError,
}

#[derive(Clone)]
pub struct StatelessServer {
    notification_reply: NotificationReply,
    abort_tools_call: bool,
    collide_ids_on_tools_call: bool,
    post_count: Arc<AtomicUsize>,
    get_count: Arc<AtomicUsize>,
    client_responses: Arc<std::sync::Mutex<Vec<Value>>>,
}

impl StatelessServer {
    pub fn new(notification_reply: NotificationReply) -> Self {
        Self {
            notification_reply,
            abort_tools_call: false,
            collide_ids_on_tools_call: false,
            post_count: Arc::new(AtomicUsize::new(0)),
            get_count: Arc::new(AtomicUsize::new(0)),
            client_responses: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    /// Answers `tools/call` with an SSE stream that first sends a
    /// *server-initiated* request reusing the client's own request id, then the
    /// response to that request under the same id. The two ids travel in
    /// opposite directions, so nothing about this is ambiguous -- but it is
    /// exactly the collision the proxy's id rewriting was said to prevent.
    pub fn colliding_ids_on_tools_call(mut self) -> Self {
        self.collide_ids_on_tools_call = true;
        self
    }

    /// JSON-RPC responses the *client* sent back to the server.
    pub fn client_responses(&self) -> Vec<Value> {
        self.client_responses.lock().unwrap().clone()
    }

    /// Answers `tools/call` by sending response headers and then dropping the
    /// connection mid-body. The connection itself is fine and every other
    /// method still works -- only this one call is unanswerable.
    pub fn aborting_tools_call(mut self) -> Self {
        self.abort_tools_call = true;
        self
    }

    pub fn post_count(&self) -> usize {
        self.post_count.load(Ordering::SeqCst)
    }

    pub fn get_count(&self) -> usize {
        self.get_count.load(Ordering::SeqCst)
    }

    pub fn router(&self) -> Router {
        Router::new()
            .route("/tidewave/mcp", any(handle))
            .with_state(self.clone())
    }

    /// Binds an ephemeral port, serves in the background, returns the URL.
    pub async fn spawn(&self) -> anyhow::Result<(String, tokio::task::JoinHandle<()>)> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let router = self.router();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Ok((format!("http://{addr}/tidewave/mcp"), handle))
    }
}

fn json_response(status: StatusCode, body: Value) -> Response {
    let mut response = Response::new(Body::from(body.to_string()));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

async fn handle(
    State(server): State<StatelessServer>,
    request: axum::extract::Request,
) -> Response {
    if request.method() != axum::http::Method::POST {
        // No server-to-client SSE channel, and no session to delete.
        server.get_count.fetch_add(1, Ordering::SeqCst);
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    server.post_count.fetch_add(1, Ordering::SeqCst);

    let bytes = match axum::body::to_bytes(request.into_body(), 1024 * 1024).await {
        Ok(bytes) => bytes,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    let message: Value = match serde_json::from_slice(&bytes) {
        Ok(message) => message,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };

    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let id = message.get("id").cloned();

    // A message with an id but no method is the client answering a
    // server-initiated request.
    if id.is_some() && message.get("method").is_none() {
        server
            .client_responses
            .lock()
            .unwrap()
            .push(message.clone());
        return StatusCode::ACCEPTED.into_response();
    }

    // A notification has no id, so there is nothing to respond with.
    let Some(id) = id else {
        return match server.notification_reply {
            NotificationReply::Accepted => StatusCode::ACCEPTED.into_response(),
            NotificationReply::AcceptedWithBody => {
                json_response(StatusCode::ACCEPTED, json!({"status": "ok"}))
            }
            NotificationReply::EmptyJsonObject => json_response(StatusCode::OK, json!({})),
            NotificationReply::TextPlain => (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                "",
            )
                .into_response(),
            NotificationReply::ServerError => (
                StatusCode::INTERNAL_SERVER_ERROR,
                [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                "Internal Server Error",
            )
                .into_response(),
            NotificationReply::NoContentType => {
                let mut response = Response::new(Body::from(""));
                response.headers_mut().remove(header::CONTENT_TYPE);
                response
            }
        };
    };

    if method == "tools/call" && server.collide_ids_on_tools_call {
        // Deliberately reuse the client's request id for a server-initiated
        // request, then answer the original request under that same id.
        let server_request = json!({"jsonrpc": "2.0", "id": id, "method": "ping"});
        let answer = json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {"content": [{"type": "text", "text": "done"}], "isError": false},
        });
        let stream =
            format!("event: message\ndata: {server_request}\n\nevent: message\ndata: {answer}\n\n");
        let mut response = Response::new(Body::from(stream));
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        );
        return response;
    }

    if method == "tools/call" && server.abort_tools_call {
        let aborting = futures::stream::once(async {
            Err::<axum::body::Bytes, std::io::Error>(std::io::Error::other("connection aborted"))
        });
        let mut response = Response::new(Body::from_stream(aborting));
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        return response;
    }

    let result = match method {
        "initialize" => json!({
            "protocolVersion": "2025-03-26",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "Stateless MCP Server", "version": "0.8.1"},
        }),
        "tools/list" => json!({
            "tools": [{
                "name": "echo",
                "description": "Echoes back the message",
                "inputSchema": {
                    "type": "object",
                    "properties": {"message": {"type": "string"}},
                    "required": ["message"],
                },
            }],
        }),
        "tools/call" => {
            let message_arg = message
                .pointer("/params/arguments/message")
                .and_then(Value::as_str)
                .unwrap_or("");
            json!({"content": [{"type": "text", "text": message_arg}], "isError": false})
        }
        "ping" => json!({}),
        _ => {
            return json_response(
                StatusCode::OK,
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": -32601, "message": format!("unknown method {method}")},
                }),
            );
        }
    };

    json_response(
        StatusCode::OK,
        json!({"jsonrpc": "2.0", "id": id, "result": result}),
    )
}
