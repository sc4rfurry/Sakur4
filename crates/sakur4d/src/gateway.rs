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
use rmcp::transport::stdio;
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
) -> Result<()> {
    let server = Sakur4Server::new(engine.clone());
    let cancel = tokio_util::sync::CancellationToken::new();
    let consolidator = spawn_consolidator(&engine, dream, quiet_secs);

    match transport {
        Transport::Stdio => serve_stdio(server, cancel).await,
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
) -> Result<()> {
    // stdout is the protocol channel; everything human-readable goes to stderr.
    eprintln!(
        "sakur4d {} — MCP over stdio (protocol {})",
        env!("CARGO_PKG_VERSION"),
        sakur4_core::MCP_PROTOCOL_VERSION
    );
    let running =
        rmcp::serve_server(server, stdio()).await.context("starting the MCP server on stdio")?;

    tokio::select! {
        result = running.waiting() => {
            result.context("the MCP stdio session ended with an error")?;
        }
        _ = cancel.cancelled() => {
            tracing::info!("shutdown requested");
        }
    }
    Ok(())
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
