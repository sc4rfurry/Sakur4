//! Sakur4 core: a cache-coherent memory and context operating system for local
//! coding and research agents.
//!
//! This crate is the engine. The MCP server that harnesses talk to lives in
//! `sakur4d` and is a thin adapter over [`Engine`], so everything here is usable
//! without a transport.
//!
//! # The problem, because it explains the shape of everything below
//!
//! An agent harness compacts when the context window fills. It replaces the
//! transcript with a summary and sends the result. That new token sequence shares
//! no prefix with the old one, so llama.cpp's longest-common-prefix slot matching
//! finds nothing and the *entire* compacted context is re-prefilled — measured at
//! 100+ seconds for a 50K-token session on consumer hardware. The operation whose
//! purpose was to make the session cheap becomes the most expensive thing in it.
//!
//! Nothing in that loop is wrong. The harness and the inference server simply do
//! not know about each other. Sakur4 knows about both.
//!
//! Three commitments follow, and they are why this crate is organised the way it
//! is:
//!
//! 1. **Memory is two tracks.** Deterministic parser facts in the [`memory::symbolic`]
//!    Ledger, model interpretation in the [`memory::semantic`] Atlas, and no path
//!    from a model into the first. A summary that has drifted from its source is
//!    caught at read time by comparing hashes, not trusted because it was written.
//! 2. **Eviction is deterministic.** [`evict`] selects what to compress from token
//!    counts, recency, graph in-degree and explicit droppability. No model is
//!    consulted, so a plan is reproducible, auditable, and cannot hallucinate.
//! 3. **Compaction is cache-aware.** [`cache`] decides where the eviction boundary
//!    may fall by asking what the inference server can actually rewind to, so the
//!    surviving prompt head stays a prefix of what the server holds.
//!
//! # Example
//!
//! Opening an engine and reading what it detected needs no GPU, no model and no
//! network — the embedded backend simulates a llama.cpp slot so the whole system
//! is exercisable on any machine.
//!
//! ```
//! use sakur4_core::{Engine, EngineConfig};
//! use sakur4_core::memory::episodic::NewEpisode;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let engine = Engine::open(EngineConfig {
//!     db_path: ":memory:".into(),
//!     backend: "embedded".into(),
//!     ..Default::default()
//! })
//! .await?;
//!
//! // What is the cache layer actually able to do here?
//! let status = engine.status().await?;
//! assert_eq!(status.backend_name, "embedded");
//! assert!(
//!     status.capabilities.can_align_boundaries(),
//!     "the embedded backend models a checkpoint ring, so alignment is available"
//! );
//!
//! // Append a turn. The Episodic Stream is append-only; this is the only write
//! // path into it.
//! let outcome = engine
//!     .memory()
//!     .commit_episode(
//!         NewEpisode::user("demo", "rename checkUser to validateUser").with_slot("0"),
//!         engine.tokens(),
//!         true,
//!         true,
//!     )
//!     .await?;
//! assert!(outcome.episode_id.starts_with("ep_"));
//! assert!(outcome.token_count > 0);
//! # Ok(())
//! # }
//! ```
//!
//! # The components
//!
//! | Module | Component | What it is |
//! |---|---|---|
//! | [`memory`] | C1 | The dual-track, multi-tier store: append-only Episodic Stream, deterministic Symbolic Ledger, anchored Semantic Atlas, Anchor Set, and the Dependency Graph they share |
//! | [`evict`] | C2 | The Graduated Eviction Engine: four tiers, dependency-graph-aware selection, and `fold`/`unfold` for agent-directed sub-contexts |
//! | [`cache`] | C3 | The Cache-Coherence Layer: checkpoint-aligned boundaries, snapshot/restore, and an honest fallback when alignment is impossible |
//! | [`llama`] | C3 | The pluggable backend trait, with llama.cpp, embedded and null implementations |
//! | [`repo`] | C4 | Repo Cortex: tree-sitter extraction, call and import graphs, a token-budgeted map, and blast-radius queries |
//! | [`recall`] | C5 | Hybrid retrieval over BM25, dense vectors and graph adjacency, reranked with staleness resolution |
//! | [`consolidate`] | C6 | The Idle Consolidator: promotion, staleness regeneration, re-embedding and cold archival |
//! | [`receipt`] | C8 | The Context Ledger Receipt: per-turn token and cache accounting |
//! | [`provider_cache`] | C8 | The same accounting for hosted providers, from their reported cache-token counts |
//! | [`prompt`] | — | The single prompt assembler, so no two components can disagree about where a token sits |
//! | [`tokens`] | — | One tokenizer abstraction, so every budget decision and every printed number agree |
//! | [`store`] | — | SQLite in WAL, schema migrations, lexical and vector search backends |
//!
//! # Guarantees that are structural rather than aspirational
//!
//! These are worth stating because each one is enforced somewhere specific, and
//! knowing where makes the code easier to change safely.
//!
//! * **The Symbolic Ledger cannot be written by a model.** [`memory::symbolic::SymbolicFact`]
//!   has exactly one constructor and it requires naming the deterministic extractor
//!   that produced the fact. `memory::symbolic` imports nothing that could reach an
//!   inference client.
//! * **Recorded content cannot be altered.** `UPDATE` and `DELETE` on episode
//!   content are blocked by database triggers, so "an evicted episode recalls
//!   byte-identically" holds for every code path, including ones not yet written.
//! * **Anchors cannot be evicted.** Eviction selects from episodes; anchors live in
//!   a different table, so the operation is not expressible.
//! * **Library code does not panic.** There are no `unwrap`, `expect` or `panic!`
//!   paths outside tests. A malformed input from a harness is a typed error.
//!
//! # Where the difficulty lives
//!
//! [`evict::EvictionEngine::plan`] and [`cache::Coherence::plan_boundary`] are the
//! most delicate code here, because getting them wrong is silent. The order of
//! operations is the design: ask the cache layer where the boundary *can* fall,
//! then evict after it. Deciding evictions first and asking the cache afterwards
//! produces a boundary at token 0, which no checkpoint can align to, and every
//! compaction reports a full re-prefill — the exact failure this crate exists to
//! remove, arrived at by its own machinery. Three versions of that logic were
//! wrong before one was right, and each left the test suite green, which is why
//! `tests/cache_coherence.rs` states the claim as executable contracts.

pub mod cache;
pub mod consolidate;
pub mod embed;
pub mod error;
pub mod evict;
pub mod ids;
pub mod llama;
pub mod memory;
pub mod prompt;
pub mod provider_cache;
pub mod recall;
pub mod receipt;
pub mod repo;
pub mod store;
pub mod tokens;

mod engine;

pub use engine::{Engine, EngineConfig, EngineStatus};

/// The MCP protocol revision Sakur4 targets.
///
/// `2026-07-28` replaced the `initialize` handshake with per-request metadata, so
/// a client naming this revision is answered by the server with per-request
/// semantics rather than a negotiated session. See `sakur4d::gateway`.
pub const MCP_PROTOCOL_VERSION: &str = "2026-07-28";

/// Schema version written into the `meta` table by the newest migration.
///
/// A store whose recorded version is lower is migrated forward on open; one that
/// is higher is refused rather than read optimistically.
pub const SCHEMA_VERSION: i64 = 1;
