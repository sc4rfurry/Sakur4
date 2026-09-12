//! Command-line surface for `sakur4d`.
//!
//! The CLI exists so that every capability the MCP gateway exposes can also be
//! driven — and inspected — from a shell. That matters for the components whose
//! whole value is being auditable: `doctor` shows what the Cache-Coherence Layer
//! actually detected, `receipts` shows whether compaction is paying for itself,
//! and `plan` shows the eviction decision *before* it is applied. None of that is
//! comfortable to read through a chat transcript.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use sakur4_core::cache::CoherenceConfig;
use sakur4_core::evict::EvictionPolicy;
use sakur4_core::memory::anchor::{AnchorKind, PinRequest};
use sakur4_core::memory::episodic::NewEpisode;
use sakur4_core::prompt::PromptParts;
use sakur4_core::recall::RecallFilters;
use sakur4_core::{Engine, EngineConfig};

/// A cache-coherent memory and context operating system for local agents.
#[derive(Debug, Parser)]
#[command(
    name = "sakur4d",
    version,
    about = "Sakur4 — memory and context OS for local coding/research agents",
    long_about = "Sakur4 sits between an agent harness and a locally-served model. It keeps \
                  memory in two tracks (deterministic facts and anchored interpretations), \
                  evicts context by dependency-graph value instead of by summarisation, and \
                  makes every compaction decision with the inference server's own KV-cache \
                  checkpoints in view."
)]
pub struct Cli {
    /// Path to the Memory Fabric store.
    #[arg(long, global = true, env = "SAKUR4_DB", default_value = "sakur4.db")]
    pub db: PathBuf,

    /// Inference backend: `auto`, `embedded`, `none`, or an HTTP base URL.
    #[arg(long, global = true, env = "SAKUR4_BACKEND", default_value = "auto")]
    pub backend: String,

    /// Repository root for Repo Cortex.
    #[arg(long, global = true, env = "SAKUR4_PROJECT_ROOT")]
    pub project_root: Option<PathBuf>,

    /// Local embedding endpoint (OpenAI-compatible).
    #[arg(long, global = true, env = "SAKUR4_EMBED_URL")]
    pub embed_url: Option<String>,

    /// Embedding model name for that endpoint.
    #[arg(long, global = true, env = "SAKUR4_EMBED_MODEL")]
    pub embed_model: Option<String>,

    /// Context window to plan against when the backend does not report one.
    #[arg(long, global = true, default_value_t = 32_768)]
    pub context_window: usize,

    /// Increase log verbosity (`-v`, `-vv`).
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Clone, Subcommand)]
pub enum Command {
    /// Run the MCP gateway (default).
    Serve {
        /// Address to bind. Defaults to localhost, per NFR-11.
        #[arg(long, default_value = "127.0.0.1:8765")]
        bind: String,
        /// Run the Idle Consolidator in the background.
        #[arg(long, default_value_t = true)]
        dream: bool,
        /// Seconds of quiet before consolidation may run.
        #[arg(long, default_value_t = 90)]
        quiet_secs: u64,
    },

    /// Print resolved configuration and component status.
    Doctor {
        /// Re-probe the backend rather than reporting the cached capabilities.
        #[arg(long)]
        refresh: bool,
    },

    /// Build or refresh the Repo Cortex index.
    Index {
        /// Repository root; defaults to `--project-root` or the working directory.
        path: Option<PathBuf>,
        /// Force a full re-parse instead of an incremental one.
        #[arg(long)]
        full: bool,
    },

    /// Print a token-budgeted structural map of the repository.
    RepoMap {
        #[arg(long, default_value_t = 2_000)]
        budget: usize,
        /// Boost symbols reachable from these paths.
        #[arg(long = "focus")]
        focus: Vec<String>,
    },

    /// Print the blast radius of changing a symbol.
    Impact {
        /// Qualified name, e.g. `src::auth::validate`.
        symbol: String,
        #[arg(long, default_value_t = 4)]
        depth: usize,
    },

    /// Look up a symbol's current deterministic signature.
    Symbol { qualified_name: String },

    /// Query the Memory Fabric.
    Recall {
        query: String,
        #[arg(long, default_value_t = 8)]
        k: usize,
        /// Restrict to one session.
        #[arg(long)]
        session: Option<String>,
        /// Include folded subtask traces.
        #[arg(long)]
        include_folded: bool,
    },

    /// Append a turn to a session's Episodic Stream.
    Commit {
        session: String,
        content: String,
        #[arg(long, default_value = "user")]
        role: String,
        #[arg(long, default_value = "0")]
        slot: String,
    },

    /// Pin a constraint into the Anchor Set.
    Pin {
        content: String,
        #[arg(long, default_value = "task_contract")]
        kind: String,
        #[arg(long)]
        session: Option<String>,
    },

    /// Show the Anchor Set.
    Anchors {
        #[arg(long)]
        session: Option<String>,
    },

    /// Show or apply an eviction plan for a session.
    Plan {
        session: String,
        #[arg(long, default_value = "0")]
        slot: String,
        /// Apply the plan instead of only printing it.
        #[arg(long)]
        apply: bool,
    },

    /// Force a KV-cache save for a slot.
    Snapshot {
        #[arg(long, default_value = "0")]
        slot: String,
        #[arg(long, default_value = "default")]
        session: String,
    },

    /// Warm-restore a slot from a save file.
    Restore {
        #[arg(long, default_value = "0")]
        slot: String,
        #[arg(long, default_value = "default")]
        session: String,
        /// Path to the save file returned by `snapshot`.
        path: String,
    },

    /// Print the Context Ledger Receipt for a session.
    Receipt {
        session: String,
        /// Print the whole history rather than only the latest.
        #[arg(long)]
        history: bool,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },

    /// Run one Idle Consolidator pass.
    Dream,

    /// Report recorded staleness between the Atlas and its anchors.
    Staleness,

    /// End-to-end walkthrough against the embedded backend.
    Demo {
        /// Repository root to index for the Repo Cortex portion.
        #[arg(long)]
        repo: Option<PathBuf>,
    },
}

/// Initialise tracing at the requested verbosity.
pub fn init_tracing(verbose: u8) {
    let level = match verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| {
        format!("sakur4={level},sakur4_core={level},sakur4d={level},warn")
    });
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_new(filter)
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_target(false)
        .compact()
        .init();
}

pub(crate) fn build_config(cli: &Cli) -> EngineConfig {
    let mut cfg = EngineConfig {
        db_path: cli.db.display().to_string(),
        backend: cli.backend.clone(),
        default_n_ctx: cli.context_window,
        embed_url: cli.embed_url.clone(),
        embed_model: cli.embed_model.clone(),
        project_root: cli
            .project_root
            .as_ref()
            .map(|p| p.display().to_string())
            .or_else(|| {
                std::env::current_dir()
                    .ok()
                    .map(|p| p.display().to_string())
            }),
        ..Default::default()
    };
    cfg.eviction = EvictionPolicy::default();
    cfg.coherence = CoherenceConfig::default();
    cfg
}

async fn open_engine(cli: &Cli) -> Result<Engine> {
    let cfg = build_config(cli);
    Engine::open(cfg)
        .await
        .context("opening the Sakur4 engine")
}

/// Dispatch a parsed command.
pub async fn run(cli: Cli) -> Result<()> {
    match cli.command.clone().unwrap_or(Command::Serve {
        bind: "127.0.0.1:8765".into(),
        dream: true,
        quiet_secs: 90,
    }) {
        Command::Serve {
            bind,
            dream,
            quiet_secs,
        } => {
            let engine = open_engine(&cli).await?;
            crate::gateway::serve(engine, &bind, dream, quiet_secs).await
        }
        Command::Doctor { refresh } => doctor(&cli, refresh).await,
        Command::Index { path, full } => index(&cli, path, full).await,
        Command::RepoMap { budget, focus } => repo_map(&cli, budget, focus).await,
        Command::Impact { symbol, depth } => impact(&cli, &symbol, depth).await,
        Command::Symbol { qualified_name } => symbol(&cli, &qualified_name).await,
        Command::Recall {
            query,
            k,
            session,
            include_folded,
        } => recall(&cli, &query, k, session, include_folded).await,
        Command::Commit {
            session,
            content,
            role,
            slot,
        } => commit(&cli, &session, &content, &role, &slot).await,
        Command::Pin {
            content,
            kind,
            session,
        } => pin(&cli, &content, &kind, session).await,
        Command::Anchors { session } => anchors(&cli, session).await,
        Command::Plan {
            session,
            slot,
            apply,
        } => plan(&cli, &session, &slot, apply).await,
        Command::Snapshot { slot, session } => snapshot(&cli, &session, &slot).await,
        Command::Restore {
            slot,
            session,
            path,
        } => restore(&cli, &session, &slot, &path).await,
        Command::Receipt {
            session,
            history,
            limit,
        } => receipt(&cli, &session, history, limit).await,
        Command::Dream => dream(&cli).await,
        Command::Staleness => staleness(&cli).await,
        Command::Demo { repo } => crate::cli::demo::run(&cli, repo).await,
    }
}

// ===========================================================================
// Commands
// ===========================================================================

async fn doctor(cli: &Cli, refresh: bool) -> Result<()> {
    let engine = open_engine(cli).await?;
    if refresh {
        engine.refresh_backend().await?;
    }
    let status = engine.status().await?;

    println!("Sakur4 {}", env!("CARGO_PKG_VERSION"));
    println!("  store            {}", status.db.path);
    println!(
        "  schema           v{} · {} · {} bytes",
        status.db.schema_version, status.db.journal_mode, status.db.size_bytes
    );
    println!(
        "  vector backend   {}",
        engine.db().vector_backend().describe()
    );
    println!(
        "  lexical index    {}",
        if status.db.fts5 {
            "FTS5 (BM25)"
        } else {
            "UNAVAILABLE — recall falls back to a LIKE scan"
        }
    );
    println!();
    println!("inference backend");
    println!("  requested        {}", engine.requested_backend());
    println!("  resolved         {} ({})", status.backend_name, status.backend_spec);
    println!("  note             {}", status.backend_note);
    println!("  capabilities     {}", status.cache_summary);
    println!("  coherence        {}", coherence_verdict(&status.capabilities));
    println!();
    println!("context management");
    println!("  tokenizer        {}", status.tokenizer);
    println!("  embedder         {}", status.embedder);
    println!("  recall paths     {}", status.recall_backends.join(", "));
    println!(
        "  eviction         trigger at {:.0}% of the window, target {:.0}%, keep {} recent tokens",
        engine.eviction().policy().trigger_ratio * 100.0,
        engine.eviction().policy().target_ratio * 100.0,
        engine.eviction().policy().keep_recent_tokens
    );
    println!("  context window   {} tokens", engine.context_window().await);
    println!();
    println!("memory fabric");
    println!("  episodes         {}", status.db.episodes);
    println!("  symbolic facts   {}", status.db.symbolic_facts);
    println!("  atlas entries    {}", status.db.semantic_entries);
    println!("  stale entries    {}", status.db.stale_entries);
    println!("  anchors          {}", status.db.anchors);
    println!("  open folds       {}", status.db.folds_open);
    println!("  repo files       {}", status.repo_files);
    Ok(())
}

fn coherence_verdict(caps: &sakur4_core::llama::CapabilitySet) -> String {
    if !caps.reachable {
        return "DISABLED — every compaction will be a full re-prefill".into();
    }
    if caps.can_align_boundaries() {
        let mut note = "checkpoint-aligned eviction boundaries available".to_string();
        if caps.partial_state_only {
            note.push_str(
                " (partial-state architecture: only durable save points are trusted)",
            );
        }
        note
    } else {
        "no checkpoint source detected — compaction will report full re-pre-fill".into()
    }
}

async fn index(cli: &Cli, path: Option<PathBuf>, full: bool) -> Result<()> {
    let engine = open_engine(cli).await?;
    let root = path
        .or_else(|| cli.project_root.clone())
        .or_else(|| std::env::current_dir().ok())
        .context("no repository root given")?;
    let report = if full {
        engine.repo().index(&root).await?
    } else {
        engine.repo().reindex(&root).await?
    };
    println!("indexed {}", root.display());
    println!("  {}", report.summary());
    for w in report.warnings.iter().take(10) {
        println!("  warning: {w}");
    }
    Ok(())
}

async fn repo_map(cli: &Cli, budget: usize, focus: Vec<String>) -> Result<()> {
    let engine = open_engine(cli).await?;
    let focus = if focus.is_empty() {
        None
    } else {
        Some(focus.as_slice())
    };
    let (map, used) = engine
        .repo()
        .repo_map(budget, focus, engine.tokens())
        .await?;
    println!("{map}");
    eprintln!("({used} tokens of a {budget}-token budget)");
    Ok(())
}

async fn impact(cli: &Cli, symbol: &str, depth: usize) -> Result<()> {
    let engine = open_engine(cli).await?;
    let report = engine.repo().impact_of_change(symbol, depth).await?;
    println!("{}", report.render());
    Ok(())
}

async fn symbol(cli: &Cli, qualified_name: &str) -> Result<()> {
    let engine = open_engine(cli).await?;
    match engine.recall().query_symbol(qualified_name).await? {
        Some(fact) => {
            println!("{}", fact.render());
            println!("  ast_hash  {}", fact.ast_hash);
            println!("  source    {}", fact.source.as_str());
            if let Some(body) = &fact.body {
                let preview: String = body.chars().take(400).collect();
                println!("  body      {preview}");
            }
            Ok(())
        }
        None => {
            anyhow::bail!(
                "symbol {qualified_name} is not in the Symbolic Ledger; run `sakur4d index` first \
                 or check the qualified name"
            )
        }
    }
}

async fn recall(
    cli: &Cli,
    query: &str,
    k: usize,
    session: Option<String>,
    include_folded: bool,
) -> Result<()> {
    let engine = open_engine(cli).await?;
    let filters = RecallFilters {
        session_id: session,
        include_folded: if include_folded { Some(true) } else { None },
        ..Default::default()
    };
    let result = engine.recall().recall(query, Some(k), Some(filters)).await?;
    if result.hits.is_empty() {
        println!("no results for {query:?}");
        return Ok(());
    }
    for (i, hit) in result.hits.iter().enumerate() {
        println!(
            "{:>2}. [{}] score {:.3}{}  ({})",
            i + 1,
            hit.kind.as_str(),
            hit.score,
            if hit.stale { "  STALE" } else { "" },
            hit.retrievers
                .iter()
                .map(|r| r.as_str())
                .collect::<Vec<_>>()
                .join("+")
        );
        for line in hit.rendered.lines().take(6) {
            println!("      {line}");
        }
        if let Some(rep) = &hit.stale_replacement {
            println!("      ↳ CURRENT VALUE: {rep}");
        }
    }
    for note in &result.notes {
        eprintln!("note: {note}");
    }
    Ok(())
}

async fn commit(
    cli: &Cli,
    session: &str,
    content: &str,
    role: &str,
    slot: &str,
) -> Result<()> {
    let engine = open_engine(cli).await?;
    let role = sakur4_core::memory::episodic::Role::parse(role)?;
    let out = engine
        .memory()
        .commit_episode(
            NewEpisode {
                session_id: session.to_string(),
                slot_id: Some(slot.to_string()),
                role,
                content: content.to_string(),
                tool_name: None,
                fold_id: None,
                droppable: false,
                meta: None,
            },
            engine.tokens(),
            true,
            true,
        )
        .await?;
    println!(
        "episode {} (seq {}) · {} tokens · symbolic: {}",
        out.episode_id, out.seq, out.token_count, out.symbolic_summary
    );
    if let Some(p) = &out.anchor_proposal {
        println!(
            "  a constraint may have been stated ({} — rule {}, confidence {:.2}):\n    {}\n  \
             pin it with `sakur4d pin <text> --kind {}` if it should survive every compaction",
            p.kind.as_str(),
            p.rule,
            p.confidence,
            p.excerpt,
            p.kind.as_str()
        );
    }
    Ok(())
}

async fn pin(cli: &Cli, content: &str, kind: &str, session: Option<String>) -> Result<()> {
    let engine = open_engine(cli).await?;
    let kind = AnchorKind::parse(kind)?;
    let mut req = PinRequest::new(kind, content);
    if let Some(s) = session {
        req = req.in_session(s);
    }
    let row = engine.memory().pin(req).await?;
    println!(
        "pinned {} as {} — this entry is now exempt from every eviction tier",
        row.anchor_id,
        row.kind.as_str()
    );
    Ok(())
}

async fn anchors(cli: &Cli, session: Option<String>) -> Result<()> {
    let engine = open_engine(cli).await?;
    let anchors = engine.memory().anchors(session.as_deref()).await?;
    if anchors.is_empty() {
        println!("the Anchor Set is empty");
        return Ok(());
    }
    let counter = engine.tokens();
    let total: usize = anchors.iter().map(|a| a.token_cost(counter)).sum();
    for a in &anchors {
        println!(
            "[{}] {}  ({} tokens, pinned by {})",
            a.kind.as_str(),
            a.content,
            a.token_cost(counter),
            a.pinned_by
        );
    }
    println!("\n{} anchor(s), {total} tokens pinned in every prompt", anchors.len());
    Ok(())
}

async fn plan(cli: &Cli, session: &str, slot: &str, apply: bool) -> Result<()> {
    let engine = open_engine(cli).await?;
    let parts = assemble_parts(&engine, session).await?;
    let window = engine.context_window().await;
    let plan = engine
        .eviction()
        .plan(session, slot, window, &parts)
        .await?;

    println!("pressure: {:?}", plan.pressure);
    println!(
        "budget {} · trigger {} · target {} · live {} (anchors {}, fixed {})",
        plan.budget, plan.threshold, plan.target, plan.live_tokens, plan.anchor_tokens, plan.fixed_tokens
    );
    println!("{}", plan.summary());
    for u in &plan.updates {
        println!(
            "  {} → {}  ({} → {} tokens)\n    {}",
            u.from.as_str(),
            u.to.as_str(),
            u.tokens_before,
            u.tokens_after,
            u.reason
        );
    }
    for note in &plan.notes {
        println!("  note: {note}");
    }

    if apply && !plan.is_empty() {
        let outcome = engine.eviction().apply(&plan, &parts).await?;
        println!("\napplied {} episode(s), {} tokens reclaimed", outcome.applied, outcome.tokens_reclaimed);
        println!("cache: {}", outcome.cache_status.headline());
        if outcome.snapshot_taken {
            println!("pre-rewrite snapshot taken");
        }
    }
    Ok(())
}

async fn snapshot(cli: &Cli, session: &str, slot: &str) -> Result<()> {
    let engine = open_engine(cli).await?;
    let out = engine.coherence().snapshot(session, slot).await?;
    println!("snapshot {} ({} bytes, {} ms)", out.snapshot_id, out.size_bytes.unwrap_or(0), out.elapsed_ms);
    if let Some(p) = out.file_path {
        println!("  file: {p}");
        println!("  restore with: sakur4d restore --slot {slot} --session {session} {p}");
    }
    Ok(())
}

async fn restore(cli: &Cli, session: &str, slot: &str, path: &str) -> Result<()> {
    let engine = open_engine(cli).await?;
    let out = engine.coherence().restore(session, slot, path).await?;
    println!(
        "restored={} in {} ms — {}",
        out.restored, out.restore_time_ms, out.detail
    );
    Ok(())
}

async fn receipt(cli: &Cli, session: &str, history: bool, limit: usize) -> Result<()> {
    let engine = open_engine(cli).await?;
    if history {
        let rows = engine.receipts().history(session, limit).await?;
        for r in &rows {
            println!("{}", r.render());
        }
    } else {
        match engine.receipts().latest(session).await? {
            Some(r) => println!("{}", r.render()),
            None => println!("no receipts recorded for session {session}"),
        }
    }
    let stats = engine.receipts().stats(Some(session)).await?;
    println!("\n{}", stats.render());
    Ok(())
}

async fn dream(cli: &Cli) -> Result<()> {
    let engine = open_engine(cli).await?;
    let cfg = sakur4_core::consolidate::ConsolidatorConfig {
        quiet_period_secs: 0,
        ..Default::default()
    };
    let c = sakur4_core::consolidate::Consolidator::new(
        engine.db().clone(),
        engine.memory().clone(),
        engine.backend().clone(),
        engine.embedder().clone(),
        engine.tokens().clone(),
        cfg,
    );
    let report = c.maybe_run().await?;
    println!("{}", report.summary());
    for note in &report.notes {
        println!("  {note}");
    }
    Ok(())
}

async fn staleness(cli: &Cli) -> Result<()> {
    let engine = open_engine(cli).await?;
    let report = engine.memory().staleness_report(None, 100).await?;
    println!(
        "{} of {} atlas entries are stale ({:.0}%), {} with a deleted anchor",
        report.stale,
        report.total,
        report.rate() * 100.0,
        report.deleted_anchors
    );
    for e in report.entries.iter().take(20) {
        println!(
            "  {} anchored to {}({}) — {:?}",
            e.atlas_id,
            e.anchor_type.as_str(),
            e.anchor_id,
            e.reason
        );
    }
    Ok(())
}

/// Assemble the prompt for a session from the Fabric, the way the gateway does.
pub(crate) async fn assemble_parts(engine: &Engine, session: &str) -> Result<PromptParts> {
    let anchors = engine.memory().anchors(Some(session)).await?;
    let anchor_block = anchors
        .iter()
        .map(|a| a.render())
        .collect::<Vec<_>>()
        .join("\n");
    let timeline = engine
        .memory()
        .timeline(session, 1_000_000, engine.tokens(), false)
        .await?;
    Ok(PromptParts::new()
        .with_system("You are a local coding agent using Sakur4 memory and context management.")
        .with_anchors(anchor_block)
        .with_timeline(timeline.rendered))
}

pub mod demo;
