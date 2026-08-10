use crate::http_client::TolerantHttpClient;
use crate::state::{AppState, BufferMode, ProxyState, ReconnectFailureReason};
use crate::{DISCONNECTED_ERROR_CODE, McpTransport, StdoutSink, TRANSPORT_SEND_ERROR_CODE};
use anyhow::Result;
use futures::FutureExt;
use futures::SinkExt;
use rmcp::model::{
    ClientJsonRpcMessage, ClientNotification, ClientRequest, ErrorData, InitializedNotification,
    InitializedNotificationMethod, RequestId, ServerJsonRpcMessage,
};
use rmcp::transport::Transport;
use rmcp::transport::streamable_http_client::StreamableHttpError;
use tracing::{debug, error, info};

pub(crate) type TransportError = <McpTransport as Transport<rmcp::RoleClient>>::Error;

/// Renders a transport error together with everything underneath it.
///
/// The outermost message is nearly useless on its own: every HTTP-level
/// failure renders as `Client error: error sending request for url (...)`,
/// whether the server was unreachable, hung up mid-response, or spoke
/// nonsense. The cause that tells them apart lives further down the
/// `source()` chain, and reporting only the top layer throws away the one
/// piece of information a reader needs.
pub(crate) fn describe_transport_error(e: &TransportError) -> String {
    let mut parts = vec![e.to_string()];
    // `StreamableHttpError::Client` does not expose its inner error as a
    // `source`, so step into it explicitly before walking the chain.
    let mut cause = match e {
        StreamableHttpError::Client(inner) => std::error::Error::source(inner),
        other => std::error::Error::source(other),
    };
    while let Some(err) = cause {
        parts.push(err.to_string());
        cause = err.source();
    }
    parts.join(": ")
}

/// Whether a failure to send means the whole session is gone, or just that one
/// request was not accepted.
///
/// Getting this wrong in either direction is costly. Treating every failure as
/// fatal — which is what the proxy used to do — tears down a working session
/// over a single bad call and leaves the client waiting on a request it will
/// never hear about. Treating a genuinely dead server as a per-request problem
/// would lose the buffering that rides out a restart.
fn is_session_fatal(e: &TransportError) -> bool {
    match e {
        // The worker is gone; nothing can be sent on this transport again.
        StreamableHttpError::TransportChannelClosed => true,
        // The session itself is the problem, so a fresh handshake is the fix.
        StreamableHttpError::SessionExpired
        | StreamableHttpError::MissingSessionIdInResponse
        | StreamableHttpError::AuthRequired(_)
        | StreamableHttpError::InsufficientScope(_) => true,
        // We could not reach the server at all — it is down, not fussy.
        // Buffering and reconnecting is exactly what should happen here.
        StreamableHttpError::Client(inner) => inner.is_connect(),
        // The exchange happened and the server did not accept this request.
        // The connection is still good; only this call failed.
        _ => false,
    }
}

/// Answers a single request that the server would not accept, leaving the
/// session untouched.
async fn reply_request_failed(
    id: &RequestId,
    method: &str,
    detail: &str,
    stdout_sink: &mut StdoutSink,
) -> Result<()> {
    let error_response = ServerJsonRpcMessage::error(
        ErrorData::new(
            TRANSPORT_SEND_ERROR_CODE,
            format!("The server did not accept this {method} request: {detail}"),
            None,
        ),
        id.clone(),
    );

    if let Err(e) = stdout_sink.send(error_response).await {
        error!("Error writing request failure to stdout: {}", e);
    }

    Ok(())
}

pub(crate) async fn reply_disconnected(id: &RequestId, stdout_sink: &mut StdoutSink) -> Result<()> {
    let error_response = ServerJsonRpcMessage::error(
        ErrorData::new(
            DISCONNECTED_ERROR_CODE,
            "Server not connected. The SSE endpoint is currently not available. Please ensure it is running and retry.".to_string(),
            None,
        ),
        id.clone(),
    );

    if let Err(e) = stdout_sink.send(error_response).await {
        error!("Error writing disconnected error response to stdout: {}", e);
    }

    Ok(())
}

pub(crate) async fn connect(app_state: &AppState) -> Result<McpTransport> {
    let mut config =
        rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig::with_uri(
            app_state.url.clone(),
        );
    config.retry_config = std::sync::Arc::new(NeverRetrySse);
    config.channel_buffer_capacity = 16;
    // Stateless servers never issue a session id and have no SSE channel; both
    // must be optional rather than required.
    config.allow_stateless = true;
    config.reinit_on_expired_session = false;

    Ok(rmcp::transport::StreamableHttpClientTransport::with_client(
        TolerantHttpClient::new(),
        config,
    ))
}

/// The `notifications/initialized` message that completes an MCP handshake.
pub(crate) fn initialized_notification() -> ClientJsonRpcMessage {
    ClientJsonRpcMessage::notification(ClientNotification::InitializedNotification(
        InitializedNotification {
            method: InitializedNotificationMethod,
            extensions: rmcp::model::Extensions::default(),
        },
    ))
}

/// Custom retry policy that never retries SSE connections.
/// We handle reconnection ourselves in the proxy logic.
#[derive(Debug, Clone, Copy)]
struct NeverRetrySse;

impl rmcp::transport::common::client_side_sse::SseRetryPolicy for NeverRetrySse {
    fn retry(&self, _current_times: usize) -> Option<std::time::Duration> {
        None
    }
}

pub(crate) async fn try_reconnect(
    app_state: &AppState,
) -> Result<McpTransport, ReconnectFailureReason> {
    let backoff = app_state.get_backoff_duration();
    info!(
        "Attempting to reconnect in {}s (attempt {})",
        backoff.as_secs(),
        app_state.connect_tries
    );

    if app_state.disconnected_too_long() {
        error!("Reconnect timeout exceeded, giving up reconnection attempts");
        return Err(ReconnectFailureReason::TimeoutExceeded);
    }

    let result = connect(app_state).await;

    match result {
        Ok(transport) => {
            info!("Successfully reconnected to server");
            Ok(transport)
        }
        Err(e) => {
            error!("Failed to reconnect: {}", e);
            Err(ReconnectFailureReason::ConnectionFailed(e))
        }
    }
}

pub(crate) async fn send_request_to_server(
    transport: &mut McpTransport,
    request: ClientJsonRpcMessage,
    stdout_sink: &mut StdoutSink,
    app_state: &mut AppState,
) -> Result<bool> {
    debug!("Sending request to server: {:?}", request);
    let method = match &request {
        ClientJsonRpcMessage::Request(req) => req.request.method().to_string(),
        _ => "unknown".to_string(),
    };
    match transport.send(request.clone()).await {
        Ok(_) => Ok(true),
        Err(e) => {
            let detail = describe_transport_error(&e);
            error!("Error sending {} to server: {}", method, detail);

            if is_session_fatal(&e) {
                app_state.handle_fatal_transport_error();
                app_state
                    .maybe_handle_message_while_disconnected(request, stdout_sink)
                    .await?;
                return Ok(false);
            }

            // The connection is fine and only this call was refused. Say so, to
            // the request that actually failed, rather than tearing the session
            // down and reporting a transport problem several steps later.
            if let ClientJsonRpcMessage::Request(req) = &request {
                reply_request_failed(&req.id, &method, &detail, stdout_sink).await?;
            } else {
                error!("Cannot report send failure for {:?}", request);
            }

            Ok(false)
        }
    }
}

pub(crate) async fn process_client_request(
    message: ClientJsonRpcMessage,
    app_state: &mut AppState,
    transport: &mut McpTransport,
    stdout_sink: &mut StdoutSink,
) -> Result<()> {
    match app_state
        .maybe_handle_message_while_disconnected(message.clone(), stdout_sink)
        .await
    {
        Err(_) => {}
        Ok(_) => return Ok(()),
    }

    match &message {
        ClientJsonRpcMessage::Request(req) => {
            if app_state.init_message.is_none() {
                if let ClientRequest::InitializeRequest(_) = req.request {
                    debug!("Stored client initialization message");
                    app_state.init_message = Some(message.clone());
                    app_state.state = ProxyState::WaitingForServerInit(req.id.clone());
                }
            }
        }
        ClientJsonRpcMessage::Notification(notification) => {
            if let ClientNotification::InitializedNotification(_) = notification.notification {
                // The proxy owns this notification: it sends its own as soon as
                // the server's initialize response arrives, because the
                // transport will not carry any other request until it has (see
                // `AppState::complete_handshake`). Forwarding the client's copy
                // too would duplicate it, so absorb it here.
                debug!("Absorbing client initialized notification; handshake already completed.");
                return Ok(());
            }
        }
        _ => {}
    }

    if let ClientJsonRpcMessage::Request(req) = &message {
        debug!("Forwarding request from stdin to server: {:?}", req);
        send_request_to_server(transport, message, stdout_sink, app_state).await?;
        return Ok(());
    }

    debug!("Forwarding message from stdin to server: {:?}", message);
    if let Err(e) = transport.send(message).await {
        error!("Error sending message to server: {}", e);
        app_state.handle_fatal_transport_error();
    }

    Ok(())
}

pub(crate) async fn process_buffered_messages(
    app_state: &mut AppState,
    transport: &mut McpTransport,
    stdout_sink: &mut StdoutSink,
) -> Result<()> {
    let buffered_messages = std::mem::take(&mut app_state.in_buf);
    debug!("Processing {} buffered messages", buffered_messages.len());

    for message in buffered_messages {
        match &message {
            ClientJsonRpcMessage::Request(req) => {
                let request_id = req.id.clone();

                if let Err(e) = transport.send(message.clone()).await {
                    let detail = describe_transport_error(&e);
                    error!("Error sending buffered request: {}", detail);
                    let error_response = ServerJsonRpcMessage::error(
                        ErrorData::new(
                            TRANSPORT_SEND_ERROR_CODE,
                            format!("Transport error: {detail}"),
                            None,
                        ),
                        request_id,
                    );
                    if let Err(write_err) = stdout_sink.send(error_response).await {
                        error!("Error writing error response to stdout: {}", write_err);
                    }
                }
            }
            _ => {
                if let Err(e) = transport.send(message.clone()).await {
                    error!("Error sending buffered message: {}", e);
                }
            }
        }
    }
    Ok(())
}

pub(crate) async fn flush_buffer_with_errors(
    app_state: &mut AppState,
    stdout_sink: &mut StdoutSink,
) -> Result<()> {
    debug!(
        "Flushing buffer with errors: {} messages",
        app_state.in_buf.len()
    );

    let buffered_messages = std::mem::take(&mut app_state.in_buf);
    app_state.buf_mode = BufferMode::Fail;

    for message in buffered_messages {
        if let ClientJsonRpcMessage::Request(request) = message {
            debug!("Sending error response for buffered request");
            reply_disconnected(&request.id, stdout_sink).await?;
        }
    }

    Ok(())
}

pub(crate) async fn initiate_post_reconnect_handshake(
    app_state: &mut AppState,
    transport: &mut McpTransport,
    stdout_sink: &mut StdoutSink,
) -> Result<bool> {
    if let Some(init_msg) = &app_state.init_message {
        let id = if let ClientJsonRpcMessage::Request(req) = init_msg {
            req.id.clone()
        } else {
            error!("Stored init_message is not a request: {:?}", init_msg);
            return Ok(false);
        };

        debug!(
            "Initiating post-reconnect handshake by sending: {:?}",
            init_msg
        );
        app_state.state = ProxyState::WaitingForServerInitHidden(id.clone());

        if let Err(e) =
            process_client_request(init_msg.clone(), app_state, transport, stdout_sink).await
        {
            info!("Error resending init message during handshake: {}", e);
            app_state.handle_fatal_transport_error();
            Ok(false)
        } else {
            Ok(true)
        }
    } else {
        error!(
            "No initialization message stored. Cannot reconnect! This indicates a critical state issue."
        );
        Err(anyhow::anyhow!(
            "Cannot perform reconnect handshake: init_message is missing"
        ))
    }
}

pub(crate) async fn send_heartbeat_if_needed(
    app_state: &AppState,
    transport: &mut McpTransport,
) -> Option<bool> {
    if app_state.last_heartbeat.elapsed() > std::time::Duration::from_secs(5) {
        debug!("Checking connection state due to inactivity...");
        match transport.receive().now_or_never() {
            Some(Some(_)) => {
                debug!("Heartbeat check: Received message/event, connection alive.");
                Some(true)
            }
            Some(None) => {
                debug!("Heartbeat check: Stream terminated, connection dead.");
                Some(false)
            }
            None => {
                debug!("Heartbeat check: No immediate message/event, assuming alive.");
                Some(true)
            }
        }
    } else {
        None
    }
}
