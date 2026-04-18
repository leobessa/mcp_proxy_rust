use anyhow::{Context, Result, anyhow};
use clap::Parser;
use futures::StreamExt;
use rmcp::{
    model::{ClientJsonRpcMessage, ErrorCode, ProtocolVersion, ServerJsonRpcMessage},
    transport::{StreamableHttpClientTransport, Transport},
};
use std::env;
use tokio::io::{Stdin, Stdout};
use tokio::time::{Duration, Instant, sleep};
use tokio_util::codec::{FramedRead, FramedWrite};
use tracing::{debug, error, info, warn};
use tracing_subscriber::FmtSubscriber;

mod cli;
mod core;
mod state;

use crate::cli::Args;
use crate::core::{connect, flush_buffer_with_errors};
use crate::state::{AppState, ProxyState};

const DISCONNECTED_ERROR_CODE: ErrorCode = ErrorCode(-32010);
const TRANSPORT_SEND_ERROR_CODE: ErrorCode = ErrorCode(-32011);

pub(crate) type McpTransport = StreamableHttpClientTransport<reqwest::Client>;

type StdinCodec = rmcp::transport::async_rw::JsonRpcMessageCodec<ClientJsonRpcMessage>;
type StdoutCodec = rmcp::transport::async_rw::JsonRpcMessageCodec<ServerJsonRpcMessage>;
type StdinStream = FramedRead<Stdin, StdinCodec>;
type StdoutSink = FramedWrite<Stdout, StdoutCodec>;

const INITIAL_CONNECT_TIMEOUT: Duration = Duration::from_secs(5 * 60);

async fn connect_with_retry(app_state: &AppState, delay: Duration) -> Result<McpTransport> {
    let start_time = Instant::now();
    let mut attempts = 0;

    loop {
        attempts += 1;
        info!("Attempting initial connection (attempt {})...", attempts);

        let result = connect(app_state).await;

        match result {
            Ok(transport) => {
                info!("Initial connection successful!");
                return Ok(transport);
            }
            Err(e) => {
                warn!("Attempt {} failed to start transport: {}", attempts, e);
            }
        }

        if start_time.elapsed() >= INITIAL_CONNECT_TIMEOUT {
            error!("Failed to connect after {} attempts over {:?}. Giving up.", attempts, INITIAL_CONNECT_TIMEOUT);
            return Err(anyhow!("Initial connection timed out after {:?}", INITIAL_CONNECT_TIMEOUT));
        }

        info!("Retrying in {:?}...", delay);
        sleep(delay).await;
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let log_level = if args.debug {
        tracing::Level::DEBUG
    } else {
        tracing::Level::INFO
    };

    let subscriber = FmtSubscriber::builder()
        .with_max_level(log_level)
        .with_writer(std::io::stderr)
        .finish();

    tracing::subscriber::set_global_default(subscriber).context("Failed to set up logging")?;

    let server_url = match args.sse_url {
        Some(url) => url,
        None => env::var("SSE_URL").context(
            "Either the URL must be passed as the first argument or the SSE_URL environment variable must be set",
        )?,
    };

    debug!("Starting MCP proxy with URL: {}", server_url);
    debug!("Max disconnected time: {:?}s", args.max_disconnected_time);

    let override_protocol_version = if let Some(version_str) = args.override_protocol_version {
        let protocol_version = match version_str.as_str() {
            "2024-11-05" => ProtocolVersion::V_2024_11_05,
            "2025-03-26" => ProtocolVersion::V_2025_03_26,
            "2025-06-18" => ProtocolVersion::V_2025_06_18,
            "2025-11-25" => ProtocolVersion::V_2025_11_25,
            _ => {
                return Err(anyhow!(
                    "Unsupported protocol version: {}. Supported versions are: 2024-11-05, 2025-03-26, 2025-06-18, 2025-11-25",
                    version_str
                ));
            }
        };
        Some(protocol_version)
    } else {
        None
    };

    let (reconnect_tx, mut reconnect_rx) = tokio::sync::mpsc::channel(10);
    let (timer_tx, mut timer_rx) = tokio::sync::mpsc::channel(10);

    let mut app_state = AppState::new(
        server_url.clone(),
        args.max_disconnected_time,
        override_protocol_version,
    );
    app_state.reconnect_tx = Some(reconnect_tx.clone());
    app_state.timer_tx = Some(timer_tx.clone());

    info!("Attempting initial connection to {}...", server_url);
    let mut transport =
        connect_with_retry(&app_state, Duration::from_secs(args.initial_retry_interval)).await?;

    info!("Connection established. Proxy operational.");
    app_state.state = ProxyState::WaitingForClientInit;

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let mut stdin_stream: StdinStream = FramedRead::new(stdin, StdinCodec::default());
    let mut stdout_sink: StdoutSink = FramedWrite::new(stdout, StdoutCodec::default());

    info!("Connected to server endpoint, starting proxy");

    let mut heartbeat_interval = tokio::time::interval(Duration::from_secs(1));

    loop {
        tokio::select! {
            biased;
            msg = stdin_stream.next() => {
                if !app_state.handle_stdin_message(msg, &mut transport, &mut stdout_sink).await? {
                    break;
                }
            }
            result = transport.receive(), if app_state.transport_valid => {
                if !app_state.handle_sse_message(result, &mut transport, &mut stdout_sink).await? {
                    break;
                }
            }
            Some(_) = reconnect_rx.recv() => {
                if let Some(new_transport) = app_state.handle_reconnect_signal(&mut stdout_sink).await? {
                    transport = new_transport;
                }
                if app_state.disconnected_too_long() {
                    error!("Giving up after failed reconnection attempts and exceeding max disconnected time.");
                    if !app_state.in_buf.is_empty() && app_state.buf_mode == state::BufferMode::Store {
                        flush_buffer_with_errors(&mut app_state, &mut stdout_sink).await?;
                    }
                    break;
                }
            }
            Some(_) = timer_rx.recv() => app_state.handle_timer_signal(&mut stdout_sink).await?,
            _ = heartbeat_interval.tick() => app_state.handle_heartbeat_tick(&mut transport).await?,
            else => break,
        }
    }

    info!("Proxy terminated");
    Ok(())
}
