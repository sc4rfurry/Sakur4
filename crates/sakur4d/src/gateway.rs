//! The MCP transport layer (PRD component C7).
//!
//! The tool surface itself lives in [`crate::tools`]; this module is only the
//! transport and lifecycle: build the server, bind it to localhost by default,
//! and run the Idle Consolidator alongside it.
//!
//! # Transport choice
//!
//! Streamable HTTP, per the PRD's interface spec. The 2026-07-28 revision made
//! the protocol core stateless, which suits a sidecar that keeps its own state in
//! SQLite: a client can reconnect, a second harness can attach to the same store,
//! and neither depends on a held-open stream. Localhost-only binding is the
//! default (NFR-11); exposing Sakur4 to a network is an explicit decision, not a
//! default.

use std::sync::Arc;

use anyhow::{Context, Result};
use rmcp::transport::streamable_http_server::{
    StreamableHttpService, session::local::LocalSessionManager,
};
use sakur4_core::consolidate::{Consolidator, ConsolidatorConfig};
use sakur4_core::Engine;

use crate::tools::Sakur4Server;

/// Run the MCP gateway until the process is asked to stop.
pub async fn serve(engine: Engine, bind: &str, dream: bool, quiet_secs: u64) -> Result<()> {
    let server = Sakur4Server::new(engine.clone());
    let cancel = tokio_util::sync::CancellationToken::new();

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
    eprintln!("  store           {}", engine.db().path().display());
    eprintln!("  backend         {} ({})", engine.backend().name(), engine.backend_note());

    let mut consolidator_handle = None;
    if dream {
        let cfg = ConsolidatorConfig {
            quiet_period_secs: quiet_secs,
            ..Default::default()
        };
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
        consolidator_handle = Some((handle, tx));
        eprintln!("  dream cycle     every pass needs {quiet_secs}s of quiet");
    }

    // Graceful shutdown on Ctrl-C, so the store is closed rather than abandoned.
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

    if let Some((handle, tx)) = consolidator_handle {
        let _ = tx.send(true);
        let _ = handle.await;
    }
    tracing::info!("Sakur4 MCP gateway stopped");
    Ok(())
}

