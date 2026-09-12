# Sakur4

A cache-coherent memory and context operating system for local coding and research
agents, implemented in Rust against `sakur4_prd.json`.

Sakur4 sits between an agent harness (Hermes, OMP, or any OpenAI-compatible tool
loop) and a locally served model. It keeps memory in two tracks — deterministic
parser facts and anchored interpretations — evicts context by dependency-graph
value instead of by summarisation, and makes every compaction decision with the
inference server's own KV-cache checkpoints in view.

---

## The problem it addresses

Three failures compound at 64K–128K context on consumer hardware:

1. **Compaction destroys the prompt cache.** A harness compacts by replacing the
   transcript with a summary. The new token sequence shares no prefix with the old
   one, llama.cpp's longest-common-prefix slot matching finds nothing, and the whole
   compacted context is re-prefilled — 100+ seconds on a 50K-token session. The
   operation whose purpose was to make the session cheap becomes the most expensive
   thing in it.
2. **Summarisation-based memory drifts.** A function the agent "remembers" as
   `checkUser(email)` is `validateUser(id)` in the file, and the build breaks.
3. **Context rot.** Reliability degrades well before the window is full, so filling
   the window uncritically is actively harmful, not merely wasteful.

Nothing in the first failure is *wrong*. The harness and the inference server simply
do not know about each other. That gap is what this project closes.

---

## What is implemented

| Component | Status | Where |
|---|---|---|
| **C1 Memory Fabric** — Episodic Stream, Symbolic Ledger, Semantic Atlas, Anchor Set, Dependency Graph | done | `crates/sakur4-core/src/memory/` |
| **C2 Graduated Eviction Engine** — four tiers, dependency-graph walk, `fold`/`unfold` | done | `crates/sakur4-core/src/evict.rs` |
| **C3 Cache-Coherence Layer** — capability probing, checkpoint-aligned boundaries, snapshot/restore, graceful fallback | done | `crates/sakur4-core/src/cache/`, `crates/sakur4-core/src/llama/` |
| **C4 Repo Cortex** — tree-sitter AST/call/import graph, incremental re-index, token-budgeted repo map, impact query | done | `crates/sakur4-core/src/repo.rs` |
| **C5 Hybrid Recall** — BM25 + dense + graph, staleness-aware rerank | done | `crates/sakur4-core/src/recall.rs` |
| **C6 Idle Consolidator** — promotion, staleness regeneration, re-embedding, cold archival | done | `crates/sakur4-core/src/consolidate.rs` |
| **C7 MCP Gateway** — 16 tools, 4 resources, 1 prompt, targeting spec 2026-07-28 | done | `crates/sakur4d/src/tools.rs`, `crates/sakur4d/src/gateway.rs` |
| **C8 Context Ledger Receipt** — per-turn token and cache accounting | done | `crates/sakur4-core/src/receipt.rs` |
| **C9 Harness Adapters** — Hermes plugin, OMP extension, generic reverse proxy | **not started** | — |

204 tests pass (`cargo test --workspace`), including integration tests that drive the
MCP tool surface over a real HTTP listener and contract tests for the
cache-coherence claim itself.

---

## Quick start

```bash
cargo build --release
```

Everything below works with no model, no GPU and no network: Sakur4 resolves to its
embedded backend when it finds no llama.cpp server, and says so in every line it
prints.

```bash
# An end-to-end walkthrough: dual-track write, staleness detection, a real
# compaction, the cache verdict, and round-trip integrity.
sakur4d demo --db :memory:

# Index a repository and inspect what the Ledger holds.
sakur4d index /path/to/repo
sakur4d repo-map --budget 2000
sakur4d impact src::auth::validate
sakur4d symbol src::auth::validate

# What did Sakur4 actually detect about the inference backend?
sakur4d doctor

# Run the MCP gateway.
sakur4d serve --bind 127.0.0.1:8765
```

### Against a real llama.cpp server

```bash
llama-server -m model.gguf -c 65536 --slots -cms 256 -ctxcp 64

sakur4d --backend http://127.0.0.1:8080 doctor
sakur4d --backend http://127.0.0.1:8080 serve
```

`--backend` accepts `auto` (probe, fall back to embedded), `embedded`, `none`, or any
base URL — including one on another machine. `doctor` prints exactly which cache
endpoints were detected and what that means for compaction.

---

## The novel part, and how to see it working

`crates/sakur4-core/tests/cache_coherence.rs` states the claim as executable
contracts. The short version, from `sakur4d demo`:

```
=== 5 · the eviction decision ===
  pressure       Compacting
  budget         32768 · trigger 24576 · target 18022
  live 24847 · anchors 69 · fixed 73
  6 episode(s), 7812 tokens reclaimed (24989 → 17177 of 18022 target).
    partial reuse — prefix survived compaction

=== 6 · applying it, and what the cache did ===
  cache: partial reuse — prefix survived compaction
    window 17114/32768 tokens (52% full)
    cache: partial-reuse
      4034 tokens reused from the LCP, 13080 prefilled (24% saved)
```

The order of operations in `EvictionEngine::boundary_and_prefix` is the whole
design: ask the cache layer where the boundary *can* fall, then evict after it.
Deciding evictions first and asking the cache afterwards produces a boundary at
token 0, which no checkpoint can align to, and every compaction reports a full
re-prefill — which is what this project exists to remove, arrived at by its own
machinery. It took three attempts to get right; each wrong version left every
existing test green, which is why those contracts exist.

**The fallback is first-class.** With `--backend none`, an unreachable server, an
older build with no `/slots`, or a sliding-window model whose checkpoints carry only
partial state, the plan is still produced, still evicts, and reports
`full-re-prefill` with the reason. Sakur4 is always correct; it is only sometimes
not optimally fast.

---

## Architecture notes worth knowing

**Two tracks, one hard rule.** The Symbolic Ledger is written only by deterministic
extractors. `SymbolicFact` has exactly one constructor, and it requires naming the
`FactSource` that produced it — tree-sitter, a structured tool-output parser, or a
command-name parser. There is no path from a model into that table. The Semantic
Atlas is the only place LLM output is stored, every row carries a mandatory anchor
plus the anchor's hash at write time, and staleness is computed by comparing against
the anchor's *current* hash at read time.

The practical consequence, visible in `sakur4d demo`:

```
[1] (symbolic_fact · score 0.760)
    src::auth::checkUser — fn checkUser(id: UserId) -> Result<User>
[2] (semantic_entry · score 0.175 · STALE)
    [STALE SUMMARY — do not trust] checkUser looks a user up by their email address
      ↳ CURRENT VALUE: src::auth::checkUser — fn checkUser(id: UserId) -> Result<User>
```

**Append-only means append-only.** `UPDATE`/`DELETE` on recorded content in
`episodic_stream` is blocked by database triggers, so FR-5's "an evicted-then-recalled
episode is bit-identical" is a property of the schema rather than of careful coding.
Eviction changes how an episode is *rendered*, never what is stored.

**Anchors are structurally immune.** Eviction selects from episodes; anchors live in
a different table. If the Anchor Set alone would exceed the budget, Sakur4 returns a
visible `BudgetOverflow` rather than silently dropping a pinned constraint.

**Nothing is assumed about the backend.** Every llama.cpp-specific call sits behind
one trait, `InferenceBackend`, and the Cache-Coherence Layer routes every decision
through a `CapabilitySet` produced by probing at connect time. `doctor` prints it.

---

## Workspace layout

```
crates/sakur4-core/     the engine: no MCP, no transport
  memory/               C1 — episodic, symbolic, semantic, anchor, dependency, fabric
  evict.rs              C2 — four tiers, dependency walk, fold/unfold
  cache/                C3 — coherence layer and its boundary plans
  llama/                C3 — backend trait, llama.cpp adapter, embedded + null backends
  repo.rs               C4 — tree-sitter extraction, call/import graph, repo map
  recall.rs             C5 — hybrid retrieval, staleness resolution, rerank
  consolidate.rs        C6 — the dream cycle
  receipt.rs            C8 — Context Ledger Receipt
  prompt.rs             the single prompt assembler every budget decision uses
crates/sakur4d/         the daemon: CLI + MCP gateway
crates/sakur4-testkit/  fixture repos, a fake llama.cpp server, a scripted session driver
docs/DESIGN.md          how each requirement is met, and the trade-offs taken
```

`tests/` at the crate roots hold the behavioural contracts:
`cache_coherence.rs` (the central claim), `repo_parsing.rs` (Ledger contents), and
`sakur4d/tests/gateway.rs` (the tool surface over real HTTP).

---

## Configuration

Environment variables (all also available as flags):

| Variable | Meaning |
|---|---|
| `SAKUR4_DB` | Memory Fabric path. `:memory:` for ephemeral. |
| `SAKUR4_BACKEND` | `auto`, `embedded`, `none`, or a base URL. |
| `SAKUR4_LLAMA_URL` | Base URL `auto` probes. Default `http://127.0.0.1:8080`. |
| `SAKUR4_LLAMA_API_KEY` | Bearer token, for a server behind auth. |
| `SAKUR4_PROJECT_ROOT` | Repository root for Repo Cortex. |
| `SAKUR4_EMBED_URL` / `SAKUR4_EMBED_MODEL` | Local OpenAI-compatible embedding endpoint. |
| `SAKUR4_SQLITE_VEC_PATH` | A `sqlite-vec` loadable extension, if you have one. |
| `SAKUR4_SNAPSHOT_DIR` | Where slot-save files go. |

Two defaults worth knowing because they are deliberate rather than arbitrary:

* **No embedding endpoint configured** → Sakur4 uses a deterministic hashing
  embedder and *says so* in `doctor`. It is lexical, not semantic. No model is ever
  downloaded, because NFR-10 forbids mandatory network egress.
* **No `sqlite-vec` extension** → similarity search runs as an exact scan in Rust.
  Exact rather than approximate, and within the PRD's 300 ms budget at the documented
  100k-entry scale. `doctor` prints which backend is live.

---

## Known gaps

* **C9 harness adapters are not implemented.** The Hermes plugin, the OMP extension
  and the generic reverse proxy are the remaining roadmap work. Everything they would
  call is reachable through MCP today.
* **No real llama.cpp server has been exercised end to end.** The adapter is built
  against the documented `/slots`, `/slots/{id}/save|restore|erase`, `/tokenize`,
  `/props` and `/metrics` contracts, with tolerant parsing for the field-name and
  payload-key variations those revisions have shipped. `crates/sakur4-testkit` runs a
  fake server that speaks those routes over real HTTP so the *client* is tested, but
  first contact with a real build is still first contact.
* **The reference hardware is not this machine.** The development box has a GTX 1050
  with 4 GB, which cannot host the PRD's 20–40B target workload. The embedded and fake
  backends exist so the logic could be built and verified anyway; the latency and
  memory numbers in the PRD's NFR section have not been measured against the real
  thing here.
* **`acceptance_criteria` covering external systems** — an MCP conformance run
  against the reference client, LoCoMo, the Endurance Benchmark — have not been run.
