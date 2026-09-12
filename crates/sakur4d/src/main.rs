//! `sakur4d` — the Sakur4 daemon and MCP gateway.
//!
//! Subcommands:
//!
//! * `serve` — run the MCP gateway over streamable HTTP (the default).
//! * `doctor` — print resolved configuration and component status.
//! * `index` — build or refresh the Repo Cortex index.
//! * `repo-map` — print a token-budgeted structural map.
//! * `impact` — print the blast radius of a symbol.
//! * `symbol` — look up a symbol's current deterministic signature.
//! * `recall` — query the Memory Fabric from the shell.
//! * `compact` — plan (and optionally apply) an eviction for a session.
//! * `snapshot` / `restore` — force a KV-cache save or warm-restore a slot.
//! * `dream` — run the Idle Consolidator once.
//! * `demo` — an end-to-end walkthrough against the embedded backend.

mod cli;
mod gateway;
mod tools;

use anyhow::Result;
use clap::Parser;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = cli::Cli::parse();
    cli::init_tracing(cli.verbose);
    cli::run(cli).await
}
