# Changelog

## Unreleased

* Bug fixes
  * Stop losing requests against stateless plain-JSON MCP servers — ones that answer every POST inline, never issue an `mcp-session-id`, and return 405 for `GET`. Response bodies are now what identify a message; an unrecognised or missing `content-type` no longer kills the transport and leaves the request unanswered forever.
  * Complete the MCP handshake from the proxy instead of relying on the client's `notifications/initialized` arriving as the very next message. A client that skips it previously had its first request silently consumed and never answered.
  * Report a failed handshake as a failed `initialize`. Previously the proxy answered `initialize` successfully and only failed on the first tool call, so clients that merely handshake (`claude mcp list`) reported a healthy connection to a server that could not serve them.
  * Treat an unavailable server-to-client SSE channel as a missing capability rather than a transport failure.
  * Answer a request the server refuses instead of tearing the session down. Previously *any* failure to send was treated as the connection dying: the proxy disconnected, buffered the request, reconnected and replayed it, so the client heard nothing until that finished and was then told `Transport error` — blaming the transport for what was one bad call. Unrelated requests in flight at the time were swept into the buffer with it. A server that is genuinely unreachable still buffers and reconnects as before.
  * Report the underlying cause of a transport failure. Every HTTP-level failure renders identically as `error sending request for url (...)`, whether the server was unreachable, hung up mid-response, or spoke nonsense; the cause that distinguishes them lives further down the error's `source()` chain and was being discarded.
  * Stop treating an unreadable response body as an empty one. A truncated response was reported as "nothing to say", silently stranding the request waiting on it.
  * Forward JSON-RPC ids untouched. The proxy used to rewrite every request id to a generated UUID and keep a map back to the original. That map was the only reason an id could ever be *unknown*, and an unknown id meant the message was dropped with nothing but a warning — silently losing a real response whenever the mapping had been cleared by a reconnect or a buffer flush. The duplicate it guarded against cannot occur: a request id is scoped to the direction the request travels, so a client id and a server id never compete. Removing it deletes the map, both mapping functions, the leak, and the `uuid` dependency.
* Tests
  * Add regression coverage for stateless plain-JSON servers across five notification-response shapes, for clients that skip `notifications/initialized`, and for the false-green `initialize`.
  * Add coverage for a server that refuses one request: it must be answered by id, name the failing method, carry the underlying cause, and leave the session intact.
  * Add coverage for a server-initiated request that deliberately reuses the client's own request id, in both directions.
* Known issues
  * Against Tidewave 0.8.1, `initialize` and `tools/list` work but `tools/call` still fails. The root cause is not yet known; the changes above should now report it accurately rather than as a `-32011` several steps removed from the fault.

## 0.3.0 (2026-04-19)

* Breaking changes
  * Remove SSE-only transport support (2024-11-05 protocol) — all connections now use Streamable HTTP
  * Remove vendored `openssl-sys` dependency (now uses rustls via reqwest)
* Enhancements
  * Upgrade `rmcp` from stale fork to upstream v1.5.0 (crates.io)
  * Upgrade `reqwest` from 0.12 to 0.13
  * Add protocol versions `2025-06-18` and `2025-11-25` to `--override-protocol-version`
  * Add elicitation support (MCP spec 2025-06-18) — proxy forwards `elicitation/create` server-to-client requests
* Tests
  * Add elicitation roundtrip smoke test
  * Migrate existing tests from SSE to Streamable HTTP

## 0.2.3 (2025-10-15)

* Bug fixes
  * Fix upstream JSON-RPC errors being handled as transport errors, causing reconnects for things like unsupported methods (https://github.com/modelcontextprotocol/rust-sdk/pull/486)

## 0.2.2 (2025-06-24)

* Bug fixes
  * Fix backoff overflow after 64 reconnect tries causing endless immediate reconnect tries

## 0.2.1 (2025-06-18)

* Enhancements
  * add `--override-protocol-version` to override the protocol version reported by the proxy

## 0.2.0 (2025-05-20)

* Enhancements
  * support streamable HTTP transport: the proxy tries to automatically detect the correct transport to use
* Bug fixes
  * fix `annotations` field being sent as `null` causing issues in Cursor (upstream bug in the SDK)

## 0.1.1 (2025-05-02)

* Refactor code to use the [Rust MCP SDK](https://github.com/modelcontextprotocol/rust-sdk).

## 0.1.0 (2025-04-29)

Initial release.
