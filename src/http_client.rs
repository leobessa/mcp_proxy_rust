//! A deliberately tolerant [`StreamableHttpClient`].
//!
//! rmcp's stock reqwest client decides what a POST response *is* from its
//! `content-type`, and turns anything unrecognised into a transport error. In
//! the worker those errors are fatal: it exits, the transport stream ends, and
//! whatever request was in flight is never answered. A stateless plain-JSON
//! server -- one that answers every POST inline, never issues an
//! `mcp-session-id`, and has no server-to-client channel -- trips this easily.
//!
//! This client inverts the priority: the **body** decides what a response is,
//! and `content-type` is only a hint. Anything that can still be understood is
//! understood, and a well-formed HTTP exchange never becomes a fatal transport
//! error.

use std::{borrow::Cow, collections::HashMap, sync::Arc};

use futures::StreamExt;
use http::{HeaderName, HeaderValue, header::WWW_AUTHENTICATE};
use reqwest::header::ACCEPT;
use rmcp::{
    model::{ClientJsonRpcMessage, ServerJsonRpcMessage},
    transport::{
        common::http_header::{EVENT_STREAM_MIME_TYPE, HEADER_SESSION_ID, JSON_MIME_TYPE},
        streamable_http_client::{
            AuthRequiredError, StreamableHttpClient, StreamableHttpError,
            StreamableHttpPostResponse,
        },
    },
};
use sse_stream::SseStream;
use tracing::{debug, warn};

/// A `reqwest::Client` that degrades instead of failing.
#[derive(Debug, Clone, Default)]
pub(crate) struct TolerantHttpClient(reqwest::Client);

impl TolerantHttpClient {
    pub(crate) fn new() -> Self {
        Self(reqwest::Client::default())
    }
}

fn starts_with(content_type: Option<&str>, mime: &str) -> bool {
    content_type.is_some_and(|ct| ct.as_bytes().starts_with(mime.as_bytes()))
}

impl StreamableHttpClient for TolerantHttpClient {
    type Error = reqwest::Error;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_token: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        let mut request = self
            .0
            .post(uri.as_ref())
            .header(ACCEPT, [EVENT_STREAM_MIME_TYPE, JSON_MIME_TYPE].join(", "));
        if let Some(auth_token) = auth_token {
            request = request.bearer_auth(auth_token);
        }
        let session_was_attached = session_id.is_some();
        if let Some(session_id) = session_id {
            request = request.header(HEADER_SESSION_ID, session_id.as_ref());
        }
        for (name, value) in custom_headers {
            request = request.header(name, value);
        }

        let response = request.json(&message).send().await?;
        let status = response.status();

        // Auth is the one case the caller genuinely cannot recover from, so it
        // stays an error rather than being downgraded.
        if status == reqwest::StatusCode::UNAUTHORIZED
            && let Some(header) = response.headers().get(WWW_AUTHENTICATE)
            && let Ok(header) = header.to_str()
        {
            return Err(StreamableHttpError::AuthRequired(AuthRequiredError::new(
                header.to_string(),
            )));
        }

        // A session the server has forgotten is reported so the caller can
        // decide whether to re-handshake. Without a session there is nothing to
        // expire, and a 404 is just a 404.
        if status == reqwest::StatusCode::NOT_FOUND && session_was_attached {
            return Err(StreamableHttpError::SessionExpired);
        }

        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .map(|ct| String::from_utf8_lossy(ct.as_bytes()).to_string());
        let response_session_id = response
            .headers()
            .get(HEADER_SESSION_ID)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        // An event stream is the only response we must not buffer.
        if starts_with(content_type.as_deref(), EVENT_STREAM_MIME_TYPE) {
            let event_stream = SseStream::from_byte_stream(response.bytes_stream()).boxed();
            return Ok(StreamableHttpPostResponse::Sse(
                event_stream,
                response_session_id,
            ));
        }

        // Everything else is read to completion, both so the body can be
        // inspected and so the connection goes back to the pool clean.
        //
        // A body that cannot be read is a real failure, not an empty body:
        // treating a truncated response as "nothing to say" would silently
        // strand the request that is waiting for it.
        let body = match response.text().await {
            Ok(body) => body,
            Err(e) => {
                warn!("could not read response body: {e}");
                return Err(StreamableHttpError::Client(e));
            }
        };

        // The body is the authority. A server that answers a request inline has
        // answered it, whatever it labelled the response as -- including with a
        // non-2xx status, which the spec explicitly allows for JSON-RPC errors.
        if !body.trim().is_empty() {
            match serde_json::from_str::<ServerJsonRpcMessage>(&body) {
                Ok(message) => {
                    if !starts_with(content_type.as_deref(), JSON_MIME_TYPE) {
                        debug!(
                            ?content_type,
                            "recovered a JSON-RPC message from a response that was not labelled as JSON"
                        );
                    }
                    return Ok(StreamableHttpPostResponse::Json(
                        message,
                        response_session_id,
                    ));
                }
                Err(e) => debug!("response body is not a JSON-RPC message: {e}"),
            }
        }

        // No usable body. If the server was happy, so are we: notifications and
        // responses-to-responses legitimately have nothing to say, and servers
        // signal that with 202, 204, 200-and-empty, or 200-and-`{"status":"ok"}`
        // interchangeably.
        if status.is_success() {
            return Ok(StreamableHttpPostResponse::Accepted);
        }

        // A non-2xx with nothing we can parse is a genuine protocol failure.
        warn!("HTTP {status} with no usable JSON-RPC body: {body}");
        Err(StreamableHttpError::UnexpectedServerResponse(Cow::Owned(
            format!("HTTP {status}: {body}"),
        )))
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        last_event_id: Option<String>,
        auth_token: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<
        futures::stream::BoxStream<'static, Result<sse_stream::Sse, sse_stream::Error>>,
        StreamableHttpError<Self::Error>,
    > {
        // A server that will not give us a server-to-client channel is a server
        // without one. That is a capability limit, not a transport failure, and
        // reporting it as `ServerDoesNotSupportSse` lets the worker carry on
        // with a POST-only session instead of tearing the connection down.
        match StreamableHttpClient::get_stream(
            &self.0,
            uri,
            session_id,
            last_event_id,
            auth_token,
            custom_headers,
        )
        .await
        {
            Ok(stream) => Ok(stream),
            Err(StreamableHttpError::ServerDoesNotSupportSse) => {
                Err(StreamableHttpError::ServerDoesNotSupportSse)
            }
            Err(e) => {
                debug!("no server-to-client stream available ({e}); continuing without one");
                Err(StreamableHttpError::ServerDoesNotSupportSse)
            }
        }
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        auth_token: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), StreamableHttpError<Self::Error>> {
        // Best effort: we are shutting down either way, and a server that does
        // not implement DELETE has nothing to clean up.
        if let Err(e) = StreamableHttpClient::delete_session(
            &self.0,
            uri,
            session_id,
            auth_token,
            custom_headers,
        )
        .await
        {
            debug!("could not delete session: {e}");
        }
        Ok(())
    }
}
