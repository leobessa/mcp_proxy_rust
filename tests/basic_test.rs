mod echo;
use echo::Echo;
use rmcp::{
    ServiceExt,
    transport::{
        ConfigureCommandExt, TokioChildProcess,
        streamable_http_server::{StreamableHttpService, session::local::LocalSessionManager},
    },
};

const BIND_ADDRESS: &str = "127.0.0.1:8099";
const TEST_SERVER_URL: &str = "http://localhost:8099/mcp";

#[tokio::test]
async fn test_proxy_connects_to_real_server() -> anyhow::Result<()> {
    let service = StreamableHttpService::new(
        || Ok(Echo::new()),
        LocalSessionManager::default().into(),
        Default::default(),
    );

    let router = axum::Router::new().nest_service("/mcp", service);
    let tcp_listener = tokio::net::TcpListener::bind(BIND_ADDRESS).await?;
    let server_handle = tokio::spawn(async move {
        axum::serve(tcp_listener, router).await.unwrap();
    });

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let transport = TokioChildProcess::new(
        tokio::process::Command::new("./target/debug/mcp-proxy").configure(|cmd| {
            cmd.arg(TEST_SERVER_URL);
        }),
    )?;

    let client = ().serve(transport).await?;
    let tools = client.list_all_tools().await?;

    assert!(tools.iter().any(|t| t.name == "echo"));

    if let Some(echo_tool) = tools.iter().find(|t| t.name.contains("echo")) {
        let result = client
            .call_tool(rmcp::model::CallToolRequestParams::new(echo_tool.name.clone())
                .with_arguments(rmcp::object!({
                    "message": "Hello, world!"
                })))
            .await?;

        let result_debug = format!("{:?}", result);
        assert!(
            result_debug.contains("Hello, world!"),
            "Expected result to contain 'Hello, world!', but got: {}",
            result_debug
        );
    } else {
        panic!("No echo tool found");
    }

    drop(client);
    server_handle.abort();

    Ok(())
}
