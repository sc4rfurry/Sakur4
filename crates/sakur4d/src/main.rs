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
    let mut cli = Cli::parse();
    // `clap` cannot tell a defaulted value from an explicitly written one, and `config` needs
    // the difference: an explicit store is left alone, the default is relocated to an absolute
    // path so a harness cannot land it in its own working directory.
    cli.db_explicit = cli::db_was_named();
    // # The default store is absolute, because the working directory is not ours to choose
    //
    // `--db` defaulted to the relative `sakur4.db`, so every invocation wrote its store
    // wherever it happened to be standing. For a command a person runs that is merely
    // surprising; for an MCP server it is wrong, because the *harness* picks the working
    // directory — Claude Desktop uses its own application folder — and `sakur4d config` then
    // baked that same relative path into the configuration it printed. Two harnesses would
    // silently build two empty memories, and `doctor` reported the relative path so the user
    // had no way to see where it went.
    //
    // Verified by running the generated stdio command from an unrelated directory: the store
    // appeared there, not in the configured project.
    //
    // An explicitly named store is untouched, in either direction.
    cli.db = cli::resolve_store(cli.db, cli.db_explicit);
    cli::init_tracing(cli.verbose);
    cli::run(cli).await
}
