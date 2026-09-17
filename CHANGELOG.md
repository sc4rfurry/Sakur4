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

### Added

**Hermes ContextEngine (`integrations/hermes-plugin/`)**

- Replaces Hermes' summarising context engine with Sakur4's eviction engine, and closes a
  loop MCP alone could not: `update_from_response` receives the provider's token accounting
  on every call, including cache reads, so prompt-cache behaviour is measured automatically
  rather than reported by hand.
- Injects the Anchor Set into every request via `select_context`, after the system prompt so
  the cacheable head is not moved.
- Exposes `sakur4_recall` through `get_tool_schemas`/`handle_tool_call`, and implements
  `__deepcopy__` because Hermes copies the engine for sub-agents.
- 44 contracts verified against a live daemon by `verify_engine.py`.

**OpenAI-compatible reverse proxy (FR-18, `sakur4d proxy`)**

- For a harness with no MCP client, no plugin system and no Agent Skills reader: point it at
  the proxy instead of `llama-server` and nothing else changes. Everything unrecognised is
  forwarded, so a route this build has never heard of gets the upstream's own answer rather
  than a 404.
- Trims an over-long transcript on the way through, leaving a marker where the removed turns
  were, and records the provider's token accounting from the response it already had to read.
- `--observe-only` forwards unchanged and only records, which is how to measure on real
  traffic before letting it act.

**Encryption at rest (FR-20)**

- `--features encryption` swaps the bundled SQLite for SQLCipher; `Db::open_encrypted`
  issues `PRAGMA key` before the file is read. Keys are 64 hex characters — a raw 256-bit
  key rather than a passphrase, so there is no derivation step to attack — and a short key
  is refused rather than stretched.
- The acceptance criterion is tested: a store opened with a key cannot be read by a plain
  connection, and the file does not begin with the SQLite header.

**`verify.mjs`**

- One entry point for every check, across Rust, Python, Node and three harnesses. It reports
  **skipped separately from passed**, because a run that skipped its live-server checks is
  not a green run, and `--require-all` turns a skip into a failure.

**Oh My Pi extension (`integrations/omp-plugin/`)**

- Nine native tools (`sakur4_commit`, `sakur4_pin`, `sakur4_recall`,
  `sakur4_symbol`, `sakur4_impact`, `sakur4_fold`, `sakur4_unfold`,
  `sakur4_receipt`, `sakur4_status`) and a `/sakur4` command.
- Hooks a tool provider cannot reach: a preamble injected once per session, memory
  retrieved and prepended per turn with its own token cost reported, provider usage
  forwarded automatically on every turn, `session_before_compact` replaced with
  Sakur4's planned eviction, and staleness reported at session shutdown.
- Compaction via the extension deliberately falls back to OMP's own behaviour when
  the daemon is unreachable, the plan is empty, or the plan would not reduce the
  context.
- `install.mjs`, which installs without a symlink (the `omp install` path fails on
  Windows with `EPERM` unless Developer Mode is on) and writes both the
  `package.json` dependency and the lockfile entry, because OMP's loader silently
  skips a lockfile entry that is in neither place while still listing the plugin as
  installed.

**Portable Agent Skill (`skills/sakur4/`)**

- An [Agent Skills](https://agentskills.io/specification) package usable from OMP,
  Claude Code, Codex, pi, and anything else that reads `~/.agents/skills/`.
- `scripts/sakur4.mjs`, a dependency-free Node CLI over the daemon: it locates
  `sakur4d` across install layouts, defaults the store to `~/.sakur4/sakur4.db`,
  and speaks MCP over stdio for the operations that exist only as tools. No shell is
  involved, so recorded content may contain anything.


### Changed

- **The eviction profile is chosen from what the backend can do.** The default policy kept a
  large working set so a checkpoint-aligned boundary had room — the right trade on a backend
  with a checkpoint ring, and the wrong one on a backend without. Measured against a real
  llama.cpp exposing no checkpoints, that cost 29% more tokens per turn and bought nothing.
  A backend with no checkpoint source now selects `window-first` (trigger 70%, target 30%),
  which brought the overhead to +1.3% while keeping 42 points of recall. `doctor` reports
  which profile is active, and `SAKUR4_EVICTION_PROFILE` overrides it.
- **Anchor injection in the OMP extension.** Its `context` hook called `memory.recall` and
  nothing else, so a pinned constraint reached the model only if the user's message happened
  to resemble it — which a short rule like "never force-push to main" never does. Anchors are
  now read from their own resource every turn. The guarantee was true of the engine and false
  of the live path, which is the only place a user can observe it.
- **Retrieval no longer fails on natural-language queries.** `sanitize_match` joined every
  term with `AND`, so a six-word question required all six to appear in a one-line fact; it
  now unions beyond two terms and lets BM25 rank. Inflected queries also missed their base
  form, so terms of six or more characters contribute a conservative stem variant. Measured:
  `"retries"` and `"how many retries does the helper take"` previously returned nothing and
  now find the fact, while an unrelated query still returns nothing.
- **The stdio server no longer prints a startup banner to stderr.** A harness capturing
  stderr collected one per session; `--banner` opts back in.
- **`--context-window` now has an effect.** The embedded backend simulates a slot and reports
  a fixed 32,768, and the engine asked the backend first — so a user's explicit setting lost
  to a number that describes nothing.

### Fixed

- A debug `eprintln!` in `ToolOutputParser::parse_any` printed a line per parse to stderr.
  Pinned by a test asserting a normal session writes nothing there at all.
- The proxy re-committed a harness's whole transcript every turn, so a stateless client's 242
  messages became 2,420 episodes over ten turns. A bounded set of content hashes now skips
  what has been seen.
- The Hermes engine matched plan updates on fields that do not exist, so compaction returned
  the messages unchanged while reporting success.


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

- 257 tests, including integration tests
  that spawn the real binary and speak JSON-RPC over its pipes, and contract tests
  for the cache-coherence claim.
- `hermes mcp test sakur4` connects and discovers all 17 tools against Hermes Agent
  18.x.

### Known limitations

- **No real llama.cpp server has been exercised end to end.** The adapter is tested
  against a fake server that speaks the documented routes over real HTTP, but first
  contact with a real build is still first contact.
- **The OMP compaction hook has not been exercised against a real compaction.** The
  tool path is verified end to end with a live model; forcing OMP past its context
  limit to observe `session_before_compact` is separate work.
- **Hermes has not been driven by a live model.** Its transport is verified
  (`hermes mcp test sakur4` discovers all 17 tools); its tool selection is not.
- **Optional encryption at rest (FR-20) is implemented but off by default.**
  Build with `--features encryption`. The default build links plain SQLite, so a store
  is readable by anyone with file access unless the feature was enabled.
- **`cargo deny` and `cargo audit` are not wired into CI.** Review `Cargo.lock`
  changes in a pull request.

[Unreleased]: https://github.com/sc4rfurry/Sakur4/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/sc4rfurry/Sakur4/releases/tag/v0.1.0
