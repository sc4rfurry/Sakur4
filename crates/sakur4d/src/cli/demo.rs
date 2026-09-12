//! `sakur4d demo` — an end-to-end walkthrough.
//!
//! The point of this command is to make the PRD's central claim *observable*
//! without a GPU, a model, or a running llama.cpp: it drives a session past its
//! context budget, shows the eviction plan the engine chose, and prints the cache
//! verdict for the resulting boundary. On a machine with no checkpoint source the
//! same code path reports a full re-prefill instead — which is the honest
//! fallback, and printing it is the demonstration that the fallback exists.
//!
//! Everything it prints comes from the same components the MCP tools call, so the
//! demo cannot drift into showing behaviour the product does not have.

use anyhow::Result;
use sakur4_core::memory::anchor::PinRequest;
use sakur4_core::memory::episodic::NewEpisode;
use sakur4_core::memory::symbolic::{FactKind, FactSource, SymbolicWrite};
use sakur4_core::recall::RecallFilters;
use sakur4_core::receipt::Receipt;

use crate::cli::{self, Cli};

/// Run the walkthrough.
pub async fn run(cli_args: &Cli, repo: Option<std::path::PathBuf>) -> Result<()> {
    let mut cfg = crate::cli::build_config(cli_args);
    // The demo is most useful with *some* checkpoint source, and the embedded
    // backend is exactly the "no server, full functionality" path. It says so in
    // every line it prints.
    if cli_args.backend == "auto" || cli_args.backend.is_empty() {
        cfg.backend = "embedded".into();
    }
    cfg.db_path = if cli_args.db.as_os_str().is_empty() {
        ":memory:".into()
    } else {
        cli_args.db.display().to_string()
    };
    let engine = sakur4_core::Engine::open(cfg).await?;

    section("1 · what Sakur4 resolved");
    let status = engine.status().await?;
    println!("  backend        {} ({})", status.backend_name, status.backend_note);
    println!("  capabilities   {}", status.cache_summary);
    println!("  tokenizer      {}", status.tokenizer);
    println!("  embedder       {}", status.embedder);
    println!("  context window {} tokens", engine.context_window().await);

    section("2 · the dual-track discipline");
    let counter = engine.tokens().clone();
    // A turn that states a constraint: the deterministic detector proposes a pin.
    let constrained = engine
        .memory()
        .commit_episode(
            NewEpisode::user(
                "demo",
                "Important rule for this repository: never force-push to main, and do not \
                 delete the migrations directory under any circumstances.",
            )
            .with_slot("0"),
            &counter,
            true,
            true,
        )
        .await?;
    println!(
        "  committed user turn · {} tokens · symbolic: {}",
        constrained.token_count, constrained.symbolic_summary
    );
    if let Some(p) = &constrained.anchor_proposal {
        println!(
            "  constraint detector proposed a pin (rule {}, confidence {:.2}): {}",
            p.rule, p.confidence, p.excerpt
        );
        let row = engine
            .memory()
            .pin(PinRequest::new(p.kind, p.excerpt.clone()).in_session("demo"))
            .await?;
        println!(
            "  pinned {} as {} — now exempt from every eviction tier",
            row.anchor_id,
            row.kind.as_str()
        );
    }

    // A tool result with real structure: it becomes deterministic facts.
    let structured = engine
        .memory()
        .commit_episode(
            NewEpisode::tool_result(
                "demo",
                "read_file",
                "{\n  \"name\": \"sakur4\",\n  \"version\": \"0.1.0\",\n  \"private\": true\n}",
            )
            .with_slot("0"),
            &counter,
            true,
            false,
        )
        .await?;
    println!(
        "  committed tool result · {} tokens · symbolic: {}",
        structured.token_count, structured.symbolic_summary
    );

    // Prose has no structure, and Sakur4 says so rather than guessing.
    let prose = engine
        .memory()
        .commit_episode(
            NewEpisode::tool_result(
                "demo",
                "run_tests",
                "All 42 tests passed. The suite took 3.2 seconds.",
            )
            .with_slot("0"),
            &counter,
            true,
            false,
        )
        .await?;
    println!("  committed unstructured result · symbolic: {}", prose.symbolic_summary);

    // A symbolic fact plus an interpretation, to show staleness detection. This
    // is the failure mode PP-2 describes: the agent "remembers" a signature that
    // has since changed.
    section("3 · staleness: an interpretation that outlived its source");
    let v1 = SymbolicWrite::new(FactKind::Function, "src::auth::checkUser")
        .at_path("src/auth.rs")
        .signature("fn checkUser(email: &str) -> bool")
        .body("fn checkUser(email: &str) -> bool { db_lookup(email) }")
        .into_fact(FactSource::TreeSitter, Some("demo".into()));
    engine.memory().upsert_facts(vec![v1.clone()]).await?;
    let entry = engine
        .memory()
        .put_semantic(
            sakur4_core::memory::semantic::SemanticWrite::on_fact(
                "checkUser looks a user up by their email address",
                v1.fact_id.clone(),
            )
            .by_model("demo-aux")
            .in_project("demo"),
        )
        .await?;
    println!("  wrote interpretation {} (anchored, not stale yet)", entry.atlas_id);

    let v2 = SymbolicWrite::new(FactKind::Function, "src::auth::checkUser")
        .at_path("src/auth.rs")
        .signature("fn checkUser(id: UserId) -> Result<User>")
        .body("fn checkUser(id: UserId) -> Result<User> { users::by_id(id) }")
        .into_fact(FactSource::TreeSitter, Some("demo".into()));
    engine.memory().upsert_facts(vec![v2]).await?;
    println!("  ...the function is then edited (signature and body both change)");

    let recalled =
        engine.recall().recall("checkUser", Some(5), Some(RecallFilters::default())).await?;
    println!("\n{}", indent(&recalled.render(), 2));
    let stale_hits = recalled.hits.iter().filter(|h| h.stale).count();
    println!("  → {stale_hits} hit(s) flagged stale, each carrying its anchor's current value");

    section("4 · long session, past the context budget");
    let window = engine.context_window().await;
    let threshold = engine.eviction().threshold_for(window);
    println!("  window {window} tokens · eviction triggers at {threshold}");
    let mut turns = 0usize;
    // Drive the session *past* the threshold, not up to it: the point of the
    // walkthrough is the compaction, so stopping one turn short would demonstrate
    // nothing.
    loop {
        let live = engine.memory().session_live_tokens("demo", &counter).await?;
        if live > threshold {
            break;
        }
        turns += 1;
        engine
            .memory()
            .commit_episode(
                NewEpisode::user(
                    "demo",
                    format!(
                        "Turn {turns}: continue the refactor of the CacheCoherenceLayer so that \
                         eviction boundaries snap onto checkpoints and the surviving prefix stays \
                         an LCP match. Keep the Anchor Set untouched and keep boundary_snap_delta \
                         within tolerance_tokens. {}",
                        "Additional context for this turn. ".repeat(40)
                    ),
                )
                .with_slot("0"),
                &counter,
                false,
                false,
            )
            .await?;
        if turns.is_multiple_of(3) {
            // Interleave bulky tool output: that is what a real session looks
            // like, and it gives the engine something worth evicting.
            engine
                .memory()
                .commit_episode(
                    NewEpisode::tool_result(
                        "demo",
                        "read_file",
                        format!("pub fn placeholder() -> usize {{ {} }}", "0 + ".repeat(700)),
                    )
                    .with_slot("0"),
                    &counter,
                    true,
                    false,
                )
                .await?;
        }
        if turns > 300 {
            break;
        }
    }
    let live_now = engine.memory().session_live_tokens("demo", &counter).await?;
    println!(
        "  committed {turns} further turns and a bulky tool result every third turn\n  \
         live context {live_now} tokens"
    );

    // Keep the simulated inference slot in step with the session.
    //
    // A real harness sends each assembled prompt to the server, so the slot's
    // position tracks the conversation. The demo commits episodes directly and
    // never talks to a model, so without this the embedded backend would still be
    // sitting at its initial position and the cache-coherence section below would
    // be reporting on a slot that has nothing to do with the session. Advancing it
    // here is what makes the alignment result mean something.
    advance_simulated_slot(&engine, live_now).await;

    section("5 · the eviction decision");
    let parts = cli::assemble_parts(&engine, "demo").await?;
    let plan = engine.eviction().plan("demo", "0", window, &parts).await?;
    println!("  pressure       {:?}", plan.pressure);
    println!(
        "  budget         {} · trigger {} · target {}",
        plan.budget, plan.threshold, plan.target
    );
    println!(
        "  live {live} · anchors {anchors} · fixed {fixed}",
        live = plan.live_tokens,
        anchors = plan.anchor_tokens,
        fixed = plan.fixed_tokens
    );
    println!("  {}", plan.summary());
    for u in plan.updates.iter().take(6) {
        println!(
            "    {} → {}  ({} → {} tokens)\n      {}",
            u.from.as_str(),
            u.to.as_str(),
            u.tokens_before,
            u.tokens_after,
            u.reason
        );
    }
    if plan.updates.len() > 6 {
        println!("    ... and {} more", plan.updates.len() - 6);
    }
    for note in plan.notes.iter().take(6) {
        println!("  note: {note}");
    }

    // The crucial assertion the PRD is built around: an anchor is never touched.
    let anchors_before = engine.memory().anchors(Some("demo")).await?;
    let evicted: std::collections::HashSet<&str> =
        plan.updates.iter().map(|u| u.episode_id.as_str()).collect();
    let anchor_ids: std::collections::HashSet<&str> =
        anchors_before.iter().map(|a| a.anchor_id.as_str()).collect();
    println!(
        "  anchor safety: {} anchor(s) pinned, {} of them in the eviction set (must be 0)",
        anchor_ids.len(),
        anchor_ids.intersection(&evicted).count()
    );

    section("6 · applying it, and what the cache did");
    if !plan.is_empty() {
        let outcome = engine.eviction().apply(&plan, &parts).await?;
        println!(
            "  applied {} episode(s), reclaimed {} tokens",
            outcome.applied, outcome.tokens_reclaimed
        );
        println!("  cache: {}", outcome.cache_status.headline());
        println!("  {}", plan.coherence.as_ref().map(|c| c.reason.clone()).unwrap_or_default());
        if outcome.snapshot_taken {
            println!("  a pre-rewrite snapshot was taken, so the prefill is never paid twice");
        }
    } else {
        println!("  nothing needed evicting");
    }

    let after = cli::assemble_parts(&engine, "demo").await?;
    let observation =
        engine.coherence().observe_prompt("demo", "0", &after.render(), &counter).await?;
    let receipt = Receipt::build("demo", Some("0"), 1, &after, &counter, window)
        .with_cache(observation.cache_status.as_str(), observation.detail.clone())
        .with_cache_numbers(observation.reused_tokens, observation.prefilled_tokens)
        .with_backend(sakur4_core::llama::InferenceBackend::name(engine.backend().as_ref()));
    println!("\n{}", indent(&receipt.render(), 2));
    engine.receipts().record(&receipt).await?;

    section("7 · round-trip integrity (FR-5)");
    let episodes = engine.memory().session_episodes("demo").await?;
    let evicted_ids: Vec<&str> = plan.updates.iter().map(|u| u.episode_id.as_str()).collect();
    let mut checked = 0usize;
    for id in &evicted_ids {
        if let Some(ep) = episodes.iter().find(|e| e.episode_id == *id) {
            let recalled = engine.recall().recall_episode(&ep.episode_id).await?;
            assert_eq!(
                recalled.rendered, ep.content,
                "an evicted episode must recall byte-identically"
            );
            checked += 1;
            if checked >= 3 {
                break;
            }
        }
    }
    println!("  recalled {checked} evicted episode(s) verbatim — content is unchanged by eviction");

    if let Some(root) = repo.or_else(|| cli_args.project_root.clone()) {
        section("8 · Repo Cortex");
        let report = engine.repo().index(&root).await?;
        println!("  indexed {} — {}", root.display(), report.summary());
        let (map, used) = engine.repo().repo_map(400, None, &counter).await?;
        println!("\n{}", indent(&map, 2));
        println!("  ({used} tokens)");
    } else {
        section("8 · Repo Cortex (skipped)");
        println!("  pass --repo <path> to index a repository and print its structural map");
    }

    section("done");
    println!(
        "  The receipt above is the same one `context.receipt` returns over MCP, and the\n  \
         eviction plan is the same one `context.plan_eviction` returns. Nothing in this\n  \
         walkthrough uses a code path the MCP tools do not."
    );
    Ok(())
}

fn section(title: &str) {
    println!("\n=== {title} ===");
}

/// Bring a *simulated* slot up to the session's current size.
///
/// Only meaningful for the embedded backend, which models a server rather than
/// being one. A real llama.cpp server manages its own position, so the trait's
/// default implementation of `note_compaction` is a no-op and this would have
/// nothing to do. Detecting that is why the demo reports which backend it is
/// talking to in section 1.
async fn advance_simulated_slot(engine: &sakur4_core::Engine, tokens: usize) {
    if engine.backend().name() == "embedded" {
        engine.backend().note_compaction("0", tokens as i64).await.ok();
    }
}

fn indent(text: &str, spaces: usize) -> String {
    let pad = " ".repeat(spaces);
    text.lines().map(|l| format!("{pad}{l}")).collect::<Vec<_>>().join("\n")
}
