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
//! * `commit` — append a turn to a session's Episodic Stream.
//! * `plan` — show (and optionally apply) an eviction decision.
//! * `snapshot` / `restore` — force a KV-cache save or warm-restore a slot.
//! * `receipt` — print the Context Ledger Receipt and its aggregate statistics.
//! * `dream` — run the Idle Consolidator once.
//! * `staleness` — report interpretations whose source has changed.
//! * `demo` — an end-to-end walkthrough against the embedded backend.
//!
//! The implementation lives in the crate's library target so the gateway's tool
//! surface can be tested over real HTTP; this binary is the CLI shell around it.

use anyhow::Result;
use clap::Parser;
use sakur4d::cli::{self, Cli};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    cli::init_tracing(cli.verbose);
    cli::run(cli).await
}
