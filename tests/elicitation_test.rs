use anyhow::Result;
use rmcp::{
    ErrorData as McpError,
    ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::*,
    schemars::JsonSchema,
    service::{RequestContext, RoleServer},
    tool, tool_handler, tool_router,
    transport::streamable_http_server::{
        StreamableHttpService, session::local::LocalSessionManager,
    },
};
use serde::{Deserialize, Serialize};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    time::timeout,
};

// --- Elicitation Echo Server ---

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct AnswerForm {
    pub answer: String,
}

rmcp::elicit_safe!(AnswerForm);

#[derive(Clone)]
pub struct ElicitServer {
    tool_router: ToolRouter<ElicitServer>,
}

impl Default for ElicitServer {
    fn default() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
impl ElicitServer {
    fn new() -> Self {
        Self::default()
    }

    #[tool(description = "Ask the user a question via elicitation")]
    async fn ask_user(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(params): Parameters<serde_json::Value>,
    ) -> Result<CallToolResult, McpError> {
        let question = params
            .get("question")
            .and_then(|v| v.as_str())
            .unwrap_or("Please provide your answer");

        let result = context
            .peer
            .elicit::<AnswerForm>(question.to_string())
            .await;

        match result {
            Ok(Some(form)) => Ok(CallToolResult::success(vec![Content::text(format!(
                "User answered: {}",
                form.answer
            ))])),
            Ok(None) => Ok(CallToolResult::success(vec![Content::text(
                "No answer provided".to_string(),
            )])),
            Err(e) => Err(McpError::new(
                ErrorCode::INTERNAL_ERROR,
                format!("Elicitation failed: {}", e),
                None,
            )),
        }
    }
}

#[tool_handler]
impl ServerHandler for ElicitServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("A server that tests elicitation".to_string())
    }
}

// --- Test Infrastructure ---

struct TestGuard {
    child: Option<tokio::process::Child>,
    server_task: Option<tokio::task::JoinHandle<()>>,
    stderr_buffer: Arc<Mutex<Vec<String>>>,
}

impl Drop for TestGuard {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("Test failed! Process stderr output:");
            for line in self.stderr_buffer.lock().unwrap().iter() {
                eprintln!("{}", line);
            }
        }
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
        }
        if let Some(task) = self.server_task.take() {
            task.abort();
        }
    }
}

fn collect_stderr(
    mut stderr_reader: BufReader<tokio::process::ChildStderr>,
) -> Arc<Mutex<Vec<String>>> {
    let stderr_buffer = Arc::new(Mutex::new(Vec::new()));
    let buffer_clone = stderr_buffer.clone();
    tokio::spawn(async move {
        let mut line = String::new();
        while let Ok(bytes_read) = stderr_reader.read_line(&mut line).await {
            if bytes_read == 0 {
                break;
            }
            buffer_clone.lock().unwrap().push(line.clone());
            line.clear();
        }
    });
    stderr_buffer
}

#[tokio::test]
async fn test_elicitation_roundtrip() -> Result<()> {
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_test_writer()
        .finish();
    let _log_guard = tracing::subscriber::set_default(subscriber);

    const BIND_ADDRESS: &str = "127.0.0.1:8187";
    let server_url = format!("http://{}/mcp", BIND_ADDRESS);

    // 1. Start the elicitation-capable server
    let service = StreamableHttpService::new(
        || Ok(ElicitServer::new()),
        LocalSessionManager::default().into(),
        Default::default(),
    );
    let router = axum::Router::new().nest_service("/mcp", service);
    let tcp_listener = tokio::net::TcpListener::bind(BIND_ADDRESS).await?;
    let server_task = tokio::spawn(async move {
        axum::serve(tcp_listener, router).await.unwrap();
    });

    tokio::time::sleep(Duration::from_millis(500)).await;

    // 2. Start the proxy
    let mut cmd = tokio::process::Command::new("./target/debug/mcp-proxy");
    cmd.arg(&server_url)
        .arg("--debug")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .stdin(std::process::Stdio::piped());

    let mut child = cmd.spawn()?;
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let stderr = BufReader::new(child.stderr.take().unwrap());
    let stderr_buffer = collect_stderr(stderr);
    let _guard = TestGuard {
        child: Some(child),
        server_task: Some(server_task),
        stderr_buffer,
    };

    // 3. Initialize the MCP session
    let init_msg = r#"{"jsonrpc":"2.0","id":"init-1","method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{"elicitation":{}},"clientInfo":{"name":"elicit-test","version":"0.1.0"}}}"#;
    stdin.write_all(init_msg.as_bytes()).await?;
    stdin.write_all(b"\n").await?;

    let mut init_response = String::new();
    timeout(Duration::from_secs(10), stdout.read_line(&mut init_response)).await??;
    tracing::info!("Init response: {}", init_response.trim());
    assert!(
        init_response.contains("\"id\":\"init-1\""),
        "Init response missing ID: {}",
        init_response
    );
    assert!(
        init_response.contains("\"result\""),
        "Init response missing result: {}",
        init_response
    );

    // Send initialized notification
    let initialized_msg = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
    stdin.write_all(initialized_msg.as_bytes()).await?;
    stdin.write_all(b"\n").await?;

    tokio::time::sleep(Duration::from_millis(100)).await;

    // 4. Call the ask_user tool -- this will trigger elicitation
    let tool_call = r#"{"jsonrpc":"2.0","id":"call-1","method":"tools/call","params":{"name":"ask_user","arguments":{"question":"What is your name?"}}}"#;
    stdin.write_all(tool_call.as_bytes()).await?;
    stdin.write_all(b"\n").await?;

    // 5. Read the elicitation/create request from STDIO
    let mut elicit_request = String::new();
    timeout(Duration::from_secs(10), stdout.read_line(&mut elicit_request)).await??;
    tracing::info!("Elicitation request: {}", elicit_request.trim());

    assert!(
        elicit_request.contains("\"method\":\"elicitation/create\""),
        "Expected elicitation/create request, got: {}",
        elicit_request
    );
    assert!(
        elicit_request.contains("What is your name?"),
        "Expected question in elicitation request, got: {}",
        elicit_request
    );

    // Extract the request ID from the elicitation request
    let elicit_json: serde_json::Value = serde_json::from_str(&elicit_request)?;
    let elicit_id = elicit_json["id"].clone();
    tracing::info!("Elicitation request ID: {}", elicit_id);

    // 6. Send the elicitation response
    let elicit_response = serde_json::json!({
        "jsonrpc": "2.0",
        "id": elicit_id,
        "result": {
            "action": "accept",
            "content": {
                "answer": "Claude"
            }
        }
    });
    let response_str = serde_json::to_string(&elicit_response)?;
    tracing::info!("Sending elicitation response: {}", response_str);
    stdin.write_all(response_str.as_bytes()).await?;
    stdin.write_all(b"\n").await?;

    // 7. Read the tools/call result
    let mut tool_result = String::new();
    timeout(Duration::from_secs(10), stdout.read_line(&mut tool_result)).await??;
    tracing::info!("Tool result: {}", tool_result.trim());

    assert!(
        tool_result.contains("\"id\":\"call-1\""),
        "Tool result missing original ID: {}",
        tool_result
    );
    assert!(
        tool_result.contains("User answered: Claude"),
        "Tool result should contain the elicited answer, got: {}",
        tool_result
    );

    tracing::info!("Elicitation roundtrip test passed!");
    Ok(())
}
