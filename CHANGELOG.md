# Changelog

All notable changes to Sakur4 are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

**What counts as the public API.** The MCP tool surface — tool names, argument
shapes, result shapes, resource URIs, and the prompt name — is the contract
harnesses depend on, and it is versioned accordingly: adding a tool is a minor
change, and renaming a field is a breaking one that needs a migration note here.
The Rust APIs in `sakur4-core` and `sakur4d` are published so the daemon has a
home on crates.io rather than as a stability promise, and may change within a
`0.x` minor release.

## [Unreleased]

Nothing yet.

## [0.1.0] - 2026-09-12

The first release. Everything below is new.

### Added

**Memory Fabric (C1)**

- Append-only Episodic Stream with database-level triggers blocking updates to
  recorded content, so "an evicted episode recalls byte-identically" is a property
  of the schema rather than of careful coding.
- Deterministic Symbolic Ledger whose single constructor requires naming the parser
  that produced a fact; there is no code path from a model into it.
- Semantic Atlas with mandatory anchoring, and staleness computed at read time
  against the anchor's current hash rather than a flag that some job must refresh.
- Anchor Set, structurally exempt from every eviction tier, with a visible
  `BudgetOverflow` error rather than a silent drop when the anchors alone exceed the
  window.
- One Dependency Graph table carrying both code edges and memory edges.
- Structured tool-output parsers (JSON, CSV, HTTP headers, unified diffs, exit
  codes) so the dual-track discipline covers research work, not only code.

**Graduated Eviction Engine (C2)**

- Four tiers (`masked`, `referenced`, `archived`, `dropped`) applied one step at a
  time, never skipping.
- Dependency-graph-aware candidate scoring, deterministic and explainable, with no
  model in the decision path.
- `memory.fold` / `memory.unfold` / `memory.recall_fold` for agent-directed
  sub-context isolation, with a checkpoint taken at fold open and a rollback on
  collapse.

**Cache-Coherence Layer (C3)**

- A single `InferenceBackend` trait with three implementations: the llama.cpp HTTP
  adapter, an embedded simulation, and a null backend for when coherence is off.
- Capability probing at connect time. The binary works against a current build with
  a full checkpoint ring, an older build with only `/slots/{id}/save`, a
  sliding-window model whose checkpoints carry partial state, a server on another
  machine, or nothing at all.
- Checkpoint-aligned eviction boundaries: the boundary is chosen by asking the cache
  layer where it *can* fall, then evicting after it.
- Pre-rewrite snapshots, `session.snapshot` / `session.restore`, and a retention
  policy that bounds snapshot sprawl.
- Graceful degradation to a logged full re-prefill, never a hard failure.

**Repo Cortex (C4)**

- tree-sitter grammars for Python, TypeScript/TSX/JavaScript, Rust and Go, plus a
  conservative extractor for JSON, TOML, YAML, SQL, shell and Markdown.
- Incremental re-indexing keyed on content hash, with qualified names derived from
  node ancestry so `Engine::new` is not recorded as `new`.
- A token-budgeted, centrality-ranked repo map whose smaller budgets return a strict
  prefix of larger ones rather than a different ranking.
- `code.impact_of_change` for transitive blast radius.

**Hybrid Recall (C5)**

- BM25, dense cosine, graph adjacency, and Atlas BM25 merged by weighted reciprocal
  rank fusion.
- Staleness-aware reranking that returns a stale entry with its anchor's current
  value attached and a note saying which to trust.

**Idle Consolidator (C6)**

- Promotion, staleness regeneration, re-embedding, and cold archival, all gated on
  every tracked slot being provably idle.
- Extractive summaries by default, so the dual-track rule holds with no auxiliary
  model resident; an optional auxiliary endpoint switches to genuine interpretation,
  still anchored.
- A promotion must actually shrink the context; a summary longer than the turn it
  replaces is skipped and reported.

**Context Ledger Receipt (C8)**

- Per-turn token accounting measured through one tokenizer, with the category sum
  checked against the measured prompt total on every receipt.
- Provider-cache accounting for hosted models: `context.record_usage` takes the
  provider's cached-token counts and returns a verdict, including `PREFIX-BROKEN`
  when a rewrite shrinks the cached prefix while the prompt grows.

**MCP Gateway (C7)**

- 17 tools, 4 resources, and 1 prompt, targeting spec 2026-07-28 with
  `ttlMs`/`cacheScope` on list responses.
- Two transports: stdio (default) and streamable HTTP.
- `sakur4d config <harness>` emits ready-to-paste configuration for Hermes, the
  Claude clients, and generic HTTP or stdio clients.

**CLI**

- `serve`, `doctor`, `config`, `index`, `repo-map`, `impact`, `symbol`, `recall`,
  `commit`, `pin`, `anchors`, `plan`, `snapshot`, `restore`, `receipt`, `dream`,
  `staleness`, `demo`.

### Verified

- 222 tests, including integration tests that spawn the real binary and speak
  JSON-RPC over its pipes, and contract tests for the cache-coherence claim.
- `hermes mcp test sakur4` connects and discovers all 17 tools against Hermes Agent
  18.x.

### Known limitations

- **OMP (Oh My Pi) 18.1.17 has no MCP client**, so it cannot use Sakur4 over MCP.
  It needs a native TypeScript extension or a reverse proxy.
- **No real llama.cpp server has been exercised end to end.** The adapter is tested
  against a fake server that speaks the documented routes over real HTTP, but first
  contact with a real build is still first contact.
- **No live agent session has been run through a harness under a real model.** The
  transport is verified; tool selection under a model's judgement is not.
- **Optional encryption at rest (FR-20) is not implemented.**
- **`cargo deny` and `cargo audit` are not wired into CI.** Review `Cargo.lock`
  changes in a pull request.

[Unreleased]: https://github.com/sakur4/sakur4/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/sakur4/sakur4/releases/tag/v0.1.0
