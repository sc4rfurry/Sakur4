//! The MCP transport layer (PRD component C7).
//!
//! The tool surface itself lives in [`crate::tools`]; this module is only the
//! transport and lifecycle: build the server, serve it over the transport the
//! harness expects, and run the Idle Consolidator alongside it.
//!
//! # Two transports, because harnesses disagree
//!
//! | Transport | How a harness reaches it | Who needs it |
//! |---|---|---|
//! | **stdio** | spawns `sakur4d` as a child process and speaks JSON-RPC over its stdin/stdout | Claude Desktop, Claude Code, most MCP clients, Hermes' default `mcp_servers` form |
//! | **streamable HTTP** | connects to a URL | Hermes' `url:` form, remote/LAN use, several concurrent harnesses against one store |
//!
//! stdio is the default because it is what a client can always do: no port to
//! pick, no server to keep alive, and the process lifetime is the client's. HTTP
//! is what a *shared* store needs — one `sakur4d` serving several sessions — and
//! it is the only option when the harness runs somewhere else.
//!
//! A detail worth stating: over stdio the log channel matters. Anything written to
//! stdout that is not JSON-RPC corrupts the protocol, so diagnostics go to stderr
//! and stdout is reserved for framing. `init_tracing` targets stderr for exactly
//! this reason.

use std::sync::Arc;

use anyhow::{Context, Result};
// stdio() is not used here: the transport is hand-wired so the ordering gate has a place to sit.
use rmcp::transport::streamable_http_server::{
    StreamableHttpService, session::local::LocalSessionManager,
};
use sakur4_core::Engine;
use sakur4_core::consolidate::{Consolidator, ConsolidatorConfig};

use crate::tools::Sakur4Server;

/// Which transport to serve on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transport {
    /// JSON-RPC over stdin/stdout. The harness spawns this process.
    Stdio,
    /// Streamable HTTP, bound to this address.
    Http(String),
}

impl Transport {
    /// Parse the `--transport` / `SAKUR4_TRANSPORT` form.
    ///
    /// Accepts `stdio`, `http`, `http://host:port`, or a bare `host:port`.
    pub fn parse(raw: &str) -> Self {
        let t = raw.trim();
        if t.eq_ignore_ascii_case("stdio") {
            return Transport::Stdio;
        }
        if let Some(rest) = t.strip_prefix("http://").or_else(|| t.strip_prefix("https://")) {
            return Transport::Http(rest.trim_end_matches('/').to_string());
        }
        if t.eq_ignore_ascii_case("http") {
            return Transport::Http("127.0.0.1:8765".into());
        }
        // A bare host:port is HTTP; anything else is a configuration mistake worth
        // naming rather than silently defaulting.
        if t.contains(':') && !t.contains(' ') {
            return Transport::Http(t.trim_end_matches('/').to_string());
        }
        Transport::Stdio
    }

    pub fn describe(&self) -> String {
        match self {
            Transport::Stdio => "stdio (JSON-RPC over stdin/stdout)".into(),
            Transport::Http(addr) => format!("streamable HTTP on http://{addr}"),
        }
    }
}

/// Serve the MCP gateway on `transport` until the client or the process stops.
pub async fn serve(
    engine: Engine,
    transport: Transport,
    dream: bool,
    quiet_secs: u64,
    banner: bool,
) -> Result<()> {
    let server = Sakur4Server::new(engine.clone());
    let cancel = tokio_util::sync::CancellationToken::new();
    let consolidator = spawn_consolidator(&engine, dream, quiet_secs);

    match transport {
        Transport::Stdio => serve_stdio(server, cancel, !banner).await,
        Transport::Http(addr) => serve_http(server, &addr, cancel).await,
    }?;

    if let Some((handle, tx)) = consolidator {
        let _ = tx.send(true);
        let _ = handle.await;
    }
    tracing::info!("Sakur4 MCP gateway stopped");
    Ok(())
}

/// Serve over stdio: the client spawns us and owns our lifetime.
async fn serve_stdio(
    server: Sakur4Server,
    cancel: tokio_util::sync::CancellationToken,
    quiet: bool,
) -> Result<()> {
    // stdout is the protocol channel, so anything human-readable goes to stderr —
    // but *nothing* should go there unasked. A harness that captures stderr (every
    // one of them does, to show diagnostics when something fails) otherwise collects
    // a banner per session that is not a diagnostic and that nobody asked for.
    //
    // A person running it by hand to debug still wants the banner, so it is available
    // rather than removed: `--verbose` prints it, and `RUST_LOG=info` prints far more.
    if !quiet {
        eprintln!(
            "sakur4d {} — MCP over stdio (protocol {})",
            env!("CARGO_PKG_VERSION"),
            sakur4_core::MCP_PROTOCOL_VERSION
        );
    }
    let (mut to_server, req_rx) = tokio::io::duplex(64 * 1024);
    let (res_tx, mut from_server) = tokio::io::duplex(256 * 1024);
    let responses = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let responses_for_drain = responses.clone();
    let responses_for_pump = responses.clone();

    let pump = tokio::spawn(async move {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
        // Requests forwarded, and responses accounted for. Both count **requests**, never frames:
        // `notifications/initialized` carries no `id` and is answered with nothing, so counting frames
        // made the pump wait for a reply that cannot exist — the off-by-one that cost fourteen attempts.
        let mut forwarded = 0usize;
        let mut seen = 0usize;
        while let Ok(Some(line)) = lines.next_line().await {
            let is_notification = serde_json::from_str::<serde_json::Value>(&line)
                .map(|v| v.get("id").is_none())
                .unwrap_or(false);
            if !is_notification {
                // # The gate, and the one case it must not apply to
                //
                // Message N+1 is not forwarded until the response to N has been written — but **only when
                // there is an N**. The first request has nothing in flight to wait for, and `forwarded == 0`
                // is exactly that case. Waiting anyway blocks before `initialize` is ever sent, so every
                // response is missing rather than out of order: identical symptoms to the off-by-one above,
                // and the reason this gate produced 0 replies of 25 when it was first added.
                if forwarded > 0 {
                    wait_for_one_more(&responses_for_pump, &mut seen).await;
                }
                forwarded += 1;
            }
            if to_server.write_all(line.as_bytes()).await.is_err()
                || to_server.write_all(b"\n").await.is_err()
                || to_server.flush().await.is_err()
            {
                break;
            }
        }
        // # End of input is not the end of output
        //
        // Closing the read side as soon as stdin ends truncated `tools/list` to zero tools while
        // `sakur4.status`, a few hundred bytes, was unaffected — the difference is response size. Every
        // forwarded request is answered before the close, bounded so a stall is logged rather than hung.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
        while responses_for_pump.load(std::sync::atomic::Ordering::SeqCst) < forwarded {
            if tokio::time::Instant::now() >= deadline {
                tracing::warn!("closing the read side with requests still unanswered");
                break;
            }
            tokio::time::sleep(std::time::Duration::from_micros(200)).await;
        }
        let _ = to_server.shutdown().await;
    });

    let drain = tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let mut stdout =
            CountingStdout { inner: tokio::io::stdout(), responses: responses_for_drain };
        let _ = tokio::io::copy(&mut from_server, &mut stdout).await;
        let _ = stdout.flush().await;
    });

    let running = rmcp::serve_server(server, (req_rx, res_tx))
        .await
        .context("starting the MCP server on stdio")?;

    tokio::select! {
        result = running.waiting() => {
            pump.abort();
            drain.abort();
            result.context("the MCP stdio session ended with an error")?;
        }
        _ = cancel.cancelled() => {
            pump.abort();
            drain.abort();
            tracing::info!("shutdown requested");
        }
    }
    Ok(())
}

/// Wait until the transport has written one more response than `seen`, then record it.
///
/// A comparison against a running total rather than a consumed permit, because **a permit can be lost**:
/// an `AtomicBool` cleared with `swap(false)` erases a store landing between the writer's store and the
/// reader's next check, and a one-slot `mpsc` with `try_send` drops a release while the slot is full. Every
/// increment of a counter is observable and nothing is ever reset.
async fn wait_for_one_more(responses: &std::sync::atomic::AtomicUsize, seen: &mut usize) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(120);
    while responses.load(std::sync::atomic::Ordering::SeqCst) <= *seen {
        if tokio::time::Instant::now() >= deadline {
            tracing::error!(
                "no response for 120s; releasing the gate so the session cannot deadlock"
            );
            break;
        }
        tokio::time::sleep(std::time::Duration::from_micros(200)).await;
    }
    *seen = responses.load(std::sync::atomic::Ordering::SeqCst);
}

/// `stdout` that counts completed responses, so both the gate and the end-of-file drain can consult it.
///
/// # Why newlines and not flushes
///
/// `tokio::io::copy` does not flush per message. From `tokio-1.53.1/src/io/util/copy.rs` it sets
/// `need_flush` after a write and flushes only when a read returns `Pending` with a full buffer, so a count
/// of flushes counts an event decided by the *reader's* behaviour rather than by a response being complete.
/// JSON-RPC over stdio is newline-delimited, so a write containing a newline completed exactly that many.
struct CountingStdout<W> {
    inner: W,
    responses: Arc<std::sync::atomic::AtomicUsize>,
}

impl<W: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for CountingStdout<W> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let result = std::pin::Pin::new(&mut self.inner).poll_write(cx, buf);
        if let std::task::Poll::Ready(Ok(written)) = &result {
            let completed = buf[..*written].iter().filter(|b| **b == b'\n').count();
            if completed > 0 {
                self.responses.fetch_add(completed, std::sync::atomic::Ordering::SeqCst);
            }
        }
        result
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Serve over streamable HTTP, with graceful shutdown on Ctrl-C.
async fn serve_http(
    server: Sakur4Server,
    bind: &str,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    let service: StreamableHttpService<Sakur4Server, LocalSessionManager> =
        StreamableHttpService::new(
            {
                let server = server.clone();
                move || Ok(server.clone())
            },
            Arc::new(LocalSessionManager::default()),
            Default::default(),
        );

    let router = axum::Router::new().fallback_service(service);
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("binding the MCP gateway to {bind}"))?;
    let addr = listener.local_addr()?;

    tracing::info!(%addr, "Sakur4 MCP gateway listening (protocol {})", sakur4_core::MCP_PROTOCOL_VERSION);
    eprintln!("sakur4d {} — MCP gateway on http://{addr}", env!("CARGO_PKG_VERSION"));
    eprintln!("  protocol        {}", sakur4_core::MCP_PROTOCOL_VERSION);
    eprintln!("  store           {}", server.engine().db().path().display());
    eprintln!(
        "  backend         {} ({})",
        server.engine().backend().name(),
        server.engine().backend_note()
    );

    let shutdown_cancel = cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            tracing::info!("shutdown requested");
            shutdown_cancel.cancel();
        }
    });

    axum::serve(listener, router)
        .with_graceful_shutdown({
            let cancel = cancel.clone();
            async move { cancel.cancelled().await }
        })
        .await
        .context("running the MCP gateway")?;
    Ok(())
}

/// Start the Idle Consolidator in the background, when enabled.
fn spawn_consolidator(
    engine: &Engine,
    dream: bool,
    quiet_secs: u64,
) -> Option<(tokio::task::JoinHandle<()>, tokio::sync::watch::Sender<bool>)> {
    if !dream {
        return None;
    }
    let cfg = ConsolidatorConfig { quiet_period_secs: quiet_secs, ..Default::default() };
    let consolidator = Consolidator::new(
        engine.db().clone(),
        engine.memory().clone(),
        engine.backend().clone(),
        engine.embedder().clone(),
        engine.tokens().clone(),
        cfg,
    );
    let (tx, rx) = tokio::sync::watch::channel(false);
    let handle = tokio::spawn(consolidator.run_loop(rx));
    tracing::debug!(quiet_secs, "dream cycle started");
    Some((handle, tx))
}

#[cfg(test)]
mod transport_tests {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    /// # A round trip over a `duplex`, before anything else is layered on it
    ///
    /// Three attempts at serialising the stdio transport have now failed, and the third broke the
    /// daemon outright. The pattern in all three was the same: build the whole mechanism, wire it
    /// into the live path, and discover that *the plumbing* was wrong from a test that could not say
    /// which part.
    ///
    /// So this is the smallest thing that can be checked: does a server served over a `duplex` answer
    /// a request written to the other end? No turnstile, no pump, no ordering — one request, one
    /// reply.
    ///
    /// It is also the check that would have caught the first two attempts. Both gave the server a
    /// read/write pair derived from the same duplex while something else read the peer end, which puts
    /// two readers on one buffer; the server then answered nothing. A single message cannot be
    /// delivered to the wrong reader and still produce a correct reply, so this fails for that — which
    /// is a far better diagnostic than "the daemon hangs".
    #[tokio::test]
    async fn a_server_over_a_duplex_answers_one_request() {
        let dir = tempfile::tempdir().expect("temp dir");
        let cfg = sakur4_core::EngineConfig {
            db_path: dir.path().join("duplex.db").to_string_lossy().to_string(),
            backend: sakur4_core::llama::BackendSpec::Embedded.to_string(),
            ..Default::default()
        };
        let engine = sakur4_core::Engine::open(cfg).await.expect("engine opens");
        let server = crate::tools::Sakur4Server::new(engine);

        // rmcp splits a combined `AsyncRead + AsyncWrite` itself, so the server takes one end whole.
        let (server_io, client_io) = tokio::io::duplex(64 * 1024);
        let serving = tokio::spawn(async move {
            let running = rmcp::serve_server(server, server_io).await.expect("server starts");
            let _ = running.waiting().await;
        });

        let (client_read, mut client_write) = tokio::io::split(client_io);
        let mut lines = tokio::io::BufReader::new(client_read).lines();

        // The 2026-07-28 revision is stateless: every request carries its protocol version and client
        // capabilities in `_meta`, with no handshake first. Omitting it is answered with
        // `-32602 request _meta is missing or has malformed required fields`, which is what this test
        // did on its first run — correctly, and with a message that named the problem.
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "sakur4.status",
                "arguments": {},
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities": {}
                }
            }
        });
        client_write.write_all(format!("{request}\n").as_bytes()).await.expect("write the request");
        client_write.flush().await.expect("flush");

        let reply = tokio::time::timeout(std::time::Duration::from_secs(10), lines.next_line())
            .await
            .expect("an answer within ten seconds — the three failed attempts hung here")
            .expect("a line")
            .expect("not end-of-stream");

        let parsed: serde_json::Value = serde_json::from_str(&reply).expect("valid JSON");
        assert_eq!(parsed["id"], 1, "the reply must answer the request: {reply}");
        assert!(parsed["result"].is_object(), "and carry a result rather than an error: {reply}");
        assert_eq!(
            parsed["result"]["structuredContent"]["protocol_version"], "2026-07-28",
            "and be this server's answer: {reply}"
        );

        serving.abort();
    }

    /// The relay, which is the shape the ordering fix needs.
    ///
    /// A message can only be held back somewhere the transport controls, so the fix needs two
    /// channels with a relay between them — the client writes to one, the relay decides when each
    /// message reaches the server, and responses come back on the other.
    ///
    /// This is that relay with **no gate**, checked before any gate is added. If the gate later breaks
    /// something, the relay is already known good — the discipline the three failed attempts skipped,
    /// each of which wired a whole mechanism into the live path and learned only that "it hangs".
    ///
    /// The awkward part is that a `duplex` is one bidirectional buffer, so a relay between two of them
    /// needs the response direction carried separately rather than by copying a channel onto itself.
    /// That is what the second pair is for.
    #[tokio::test]
    async fn a_relay_carries_requests_in_and_answers_out() {
        let dir = tempfile::tempdir().expect("temp dir");
        let cfg = sakur4_core::EngineConfig {
            db_path: dir.path().join("relay.db").to_string_lossy().to_string(),
            backend: sakur4_core::llama::BackendSpec::Embedded.to_string(),
            ..Default::default()
        };
        let engine = sakur4_core::Engine::open(cfg).await.expect("engine opens");
        let server = crate::tools::Sakur4Server::new(engine);

        // # No relay, and no splitting the server's own channel
        //
        // The server reads requests from one channel and writes responses to another, which is the
        // shape `IntoTransport` accepts as a pair. That is the whole mechanism the ordering fix needs:
        // a pump decides when each request line reaches `to_server`, and the client reads answers from
        // `from_server`. No bidirectional channel to split, which is what the previous three attempts
        // got wrong — each split the server's own duplex while something else read the peer end, and
        // the server then had two readers on one buffer and answered nothing.
        let (mut to_server, req_rx) = tokio::io::duplex(64 * 1024);
        let (res_tx, mut from_server) = tokio::io::duplex(64 * 1024);

        let serving = tokio::spawn(async move {
            let running =
                rmcp::serve_server(server, (req_rx, res_tx)).await.expect("server starts");
            let _ = running.waiting().await;
        });

        let meta = serde_json::json!({
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientCapabilities": {}
        });
        for id in [1, 2] {
            let request = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": { "name": "sakur4.status", "arguments": {}, "_meta": meta }
            });
            to_server
                .write_all(format!("{request}\n").as_bytes())
                .await
                .expect("write the request");
            to_server.flush().await.expect("flush");
        }

        let mut lines = tokio::io::BufReader::new(&mut from_server).lines();
        let mut ids = Vec::new();
        for _ in 0..2 {
            let line = tokio::time::timeout(std::time::Duration::from_secs(10), lines.next_line())
                .await
                .expect("an answer within ten seconds")
                .expect("a line")
                .expect("not end-of-stream");
            let parsed: serde_json::Value = serde_json::from_str(&line).expect("valid JSON");
            assert!(parsed["result"].is_object(), "a result, not an error: {line}");
            ids.push(parsed["id"].as_i64().expect("an id"));
        }
        assert_eq!(ids, vec![1, 2], "both answers arrive, in the order asked");

        serving.abort();
    }
}
