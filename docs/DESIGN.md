# Sakur4 design notes

This document records how each requirement is met, and — more usefully — the trade-offs and
corrections taken along the way. Requirements that are *not* met are listed at the end rather than
omitted.

**The requirements are listed here rather than in a separate specification.** There was a
`sakur4_prd.json` in this repository: the document this was originally written against. It was
removed from the published tree, and this is why.

It was a **draft for review** — one person's plan, not a contract — and the implementation
diverged from it in several places that mattered. It contained a section comparing Sakur4
favourably against five named competing projects, which is an argument a repository should make on
its merits rather than assert in a specification. It described benchmarks that were never run while
the ones that *were* run are in [bench/](bench/). And it was 72 KB of generated JSON that nothing
could check, because nothing referenced the requirements in a way that would fail.

What a reader of this repository actually needs is the requirement, what the code does about it,
and how to verify it — which is the table below and the sections after it. Removing the draft made
that the single source rather than one of two.

### Requirements and where each is met

| | Requirement | Where |
|---|---|---|
| FR-1 | Append-only Episodic Stream | `store/schema.rs` triggers; `tests/fabric_contracts.rs` |
| FR-2 | Deterministic Symbolic Ledger population | `memory/symbolic.rs`; `tests/repo_parsing.rs` |
| FR-3 | Semantic Atlas anchoring | `memory/semantic.rs`, `memory/anchor.rs` |
| FR-4 | Anchor Set survives all compaction | `memory/anchor.rs`, `evict.rs`; `verify_engine.py` |
| FR-5 | Graduated eviction tiers | `evict.rs`; the tier ladder section below |
| FR-6 | Agent-directed context folding | `memory/` folds; `sakur4_fold` / `sakur4_unfold` |
| FR-7 | Checkpoint-aligned eviction boundaries | `cache/plan.rs`; *not verifiable on a server with no checkpoint ring* |
| FR-8 | Session snapshot and warm restore | `session.snapshot` / `session.restore`; `snapshot-roundtrip.mjs` |
| FR-9 | Incremental structural indexing | `repo.rs` natural-key upsert; measured at 10,000 files |
| FR-10 | Token-budgeted repo map | `repo.rs`; `map --names` for the qualified-name mode |
| FR-11 | Blast-radius / impact query | `repo.rs::impact_of_change`; `sakur4_impact` |
| FR-12 | Hybrid retrieval with staleness-aware rerank | `recall.rs`, `store/fts.rs` |
| FR-13 | Idle-triggered consolidation | `consolidate.rs`; `dream` |
| FR-14 | Spec-compliant MCP server surface | `tools.rs`; 17 tools, `stdio_transport.rs` |
| FR-15 | Per-turn token and cache accounting | `provider_cache.rs`; `usage-roundtrip.mjs` |
| FR-16 | Hermes Agent context-engine plugin | `integrations/hermes-plugin/`; 44 contracts |
| FR-17 | OMP extension and skill | `integrations/omp-plugin/`, `skills/sakur4/` |
| FR-18 | Generic reverse-proxy mode | `sakur4d proxy`; 10 contracts |
| FR-19 | Fully local operation | No mandatory egress; the suite runs offline |
| FR-20 | Optional encryption at rest | `encryption` feature; `encryption_at_rest.rs` |

`verify.mjs` runs every automatable check among those in one command, and reports skipped
separately from passed.

---

## The central bet: cache-coherent compaction (C3, FR-7, INN-1)

### The problem, restated precisely

llama.cpp keeps a KV cache per slot and matches a new prompt against it by
longest common prefix. A harness that compacts by summarising produces a prompt
whose *first* token differs from the previous one, so the match length is zero and
the entire compacted context is re-prefilled.

The insight Sakur4 is built on: a compaction can only save prefill work if the head
of the new prompt is byte-identical to the head of the old one. So the question is
not "which episodes are least valuable" but "where can the head be cut such that the
server already holds everything before it".

### The algorithm

`EvictionEngine::boundary_and_prefix` runs before any eviction is chosen:

1. Propose a boundary `cache_prefix_reserve_tokens` into the session.
2. Ask the coherence layer for the checkpoints it can actually rewind to —
   the backend's live ring merged with Sakur4's own recorded save points, filtered by
   what the model architecture permits.
3. Snap the proposal onto the nearest checkpoint at or below it, within tolerance.
4. If nothing usable lies below the proposal, align *forward* onto the oldest
   checkpoint the ring still holds, provided it is early enough to leave a middle
   worth evicting.
5. Preserve every episode before that boundary verbatim; absorb the pressure from
   what follows.

The resulting `BoundaryPlan` carries the status, the boundary, and the reason — the
reason is what the receipt prints, so a slow turn is explainable rather than
mysterious.

### Three corrections worth recording

Each of these left every existing test green when it was wrong. They are now covered
by contracts in `crates/sakur4-core/tests/cache_coherence.rs`.

**1. The order was inverted.** The first implementation chose evictions greedily from
the oldest turn and then asked the cache whether the resulting boundary happened to
line up. It never did: the boundary landed at token ~0, no checkpoint exists there,
and every compaction reported a full re-prefill. Sakur4 had reproduced the exact
failure it was written to remove, using its own machinery.

**2. A wrapped ring was treated as "nothing alignable".** A checkpoint ring only
spans `interval × depth` tokens, so its oldest entry is usually well past a
freshly-proposed boundary. Reporting a full re-prefill there is wrong: the tokens
before the oldest checkpoint are genuinely uncached, but everything from it onward is
reusable, and that is most of the prompt. The fix aligns forward onto the oldest
checkpoint when it is still early enough.

**3. The plan reported the boundary it aimed for, not the one it produced.** Episodes
are the unit of eviction, so the preserved prefix ends where an episode ends, which
is rarely exactly where a checkpoint sits. Reporting the target put a number in the
receipt that no prompt ever had. The fix measures the outcome and revises the plan.

A fourth, quieter issue: `PromptParts::timeline_tokens` estimated tokens as
`chars / 4` while the receipt measured them. The eviction engine therefore decided
"relaxed" while the receipt printed a window three-quarters full. Both now measure
through the same tokenizer.

### The fallback is a first-class path (NFR-7)

`BoundaryPlan::full_rewrite` is returned — with its reason — for: an unreachable
backend, a build with no `/slots`, a build whose ring is unusable, and
sliding-window/hybrid models whose checkpoints carry only partial state. The plan
still evicts; it reports `full-re-prefill` and says why. The original risk register
rates this integration as the project's highest risk, so the failure mode was made
"slower, and honest about it" from the start.

`partial_state_only` detection keys on architecture substrings in the model path
(`swa`, `sliding`, `gemma3`, `recurrent`, `mamba`, `hybrid`, `jamba`, `qwen3next`,
`lfm2`, `granite`). When it fires, only durable save points are trusted for
alignment; a ring rewind is not attempted, because it is not guaranteed to reproduce
the same outputs.

---

## Dual-track discipline (C1, FR-2, FR-3, INN-2, INN-4)

### FR-2 is enforced by types and by the module graph

`SymbolicFact` has one constructor, `from_deterministic_source`, and it requires a
`FactSource` — a closed enum of deterministic extractors. There is no `From<&str>`,
no general `new`, and no way to obtain a `FactSource` from a model. The write path in
`memory::symbolic` imports nothing from `embed`, `llama` or `consolidate`.

Structured tool output counts as symbolic too, which is how the original plan's "over-indexing
on code leaves research under-served" risk is addressed without special-casing:
`ToolOutputParser` handles JSON (parsed, not scanned), CSV, HTTP headers, unified
diffs and exit codes. Prose has no symbolic anchor, and `parse_any` returns an empty
fact set with `parser: None` and a summary saying "no structure detected" rather than
guessing — the original plan is explicit that Sakur4 must be honest about where the guarantee
does not apply.

### FR-3 and INN-4

Every Semantic Atlas row carries `anchor_type`, `anchor_id`, and the anchor's hash at
write time. `put_semantic` reads the anchor *first*: a missing anchor is an error, not
a row with a null hash. Staleness is a live comparison in the `semantic_atlas_staleness`
view, so a summary is stale the moment its source changes, with no background job
required to notice.

At retrieval time, `recall` attaches the anchor's *current* value to any stale hit and
labels it `[STALE SUMMARY — do not trust]`. Down-ranking alone would not be enough:
the failure mode is a confidently wrong summary, so the entry's authority is replaced
rather than merely lowered.

### A natural-key bug that mattered

SQLite treats `NULL` as distinct in a unique index. Facts with no project or no file
therefore never conflicted, so an incremental re-index *inserted duplicates* instead
of refreshing rows — and a summary's anchor kept pointing at the stale one. The
natural key now folds NULL to the empty string in both the index and the
`ON CONFLICT` target. This was invisible in the read path and fatal to staleness.

---

## Graduated eviction (C2, FR-5, FR-6, INN-3)

Four tiers: `masked` → `referenced` → `archived` → `dropped`. The engine escalates one
step at a time and never skips, which is FR-5's first acceptance criterion expressed
as control flow rather than as a check.

`dropped` requires both that the producer marked the episode droppable *and* that
nothing depends on it in the Dependency Graph. `allow_drop` defaults to false, so the
floor is `archived` unless an operator opts in.

Candidate scoring is deterministic and explainable: recency, role, in-degree in the
dependency graph, size, supersession, and explicit droppability. No model is
consulted, so a plan is reproducible and auditable — which is the point of replacing
summarisation rather than improving it.

**Round-trip integrity** (FR-5's second criterion) holds structurally: eviction writes
only to `eviction_tier`, and `UPDATE`/`DELETE` on recorded content is blocked by
database triggers. There is no code path that could alter an episode's text.

**Fold/unfold** (FR-6) takes a save point at open, records the token position as a
checkpoint, tags episodes into the fold, and on unfold collapses them to
`referenced`, commits one summary episode, and rolls the slot back to the pre-fold
checkpoint — preferring an in-memory ring rewind, falling back to a slot restore, and
reporting honestly when neither was possible.

---

## Repo Cortex (C4, FR-9, FR-10, FR-11)

tree-sitter grammars for Python, TypeScript/TSX/JavaScript, Rust and Go; a
conservative extractor for JSON, TOML, YAML, SQL, shell and Markdown; and files it
cannot parse are indexed as `unsupported` rather than silently skipped.

Qualified names come from the node's *ancestors* (`scope_of`), not from a mutable
stack threaded through the recursion. The stack version was wrong in a way that took
far too long to find: a missed pop silently mis-qualified later siblings, so
`Engine::new` was recorded as `new`. Ancestry is a property of the tree, so there is
nothing to keep in sync.

Cross-file call resolution goes through a short-name index with same-module
preference. An earlier version reconstructed qualified names by prefixing the file
path a second time, matched nothing, and produced a repository with an AST and no call
graph — `code.impact_of_change` returned an empty list while looking perfectly
healthy.

**FR-10's monotonicity** — a smaller budget returns a strict prefix, not a different
ranking — follows from computing the order once before applying any budget. The
budget contract is that `tokens_used` never exceeds `token_budget`; the one exception
is a budget too small to hold even the header and footer, which returns an explicit
"raise the budget" note rather than an empty map that looks like an empty repository.

---

## Retrieval, consolidation, receipts (C5, C6, C8)

**Hybrid recall** merges BM25, dense cosine and graph adjacency by weighted reciprocal
rank fusion. RRF rather than score normalisation, because BM25, cosine similarity and
graph distance are not on comparable scales and any normalisation between them would
be an arbitrary constant dressed up as a measurement.

The Semantic Atlas has its own retriever, added after a bug: graph traversal from a
stream hit goes `episode → fact`, never reaching an entry *derived from* it, and dense
retrieval only finds entries that were embedded. Summaries were therefore nearly
invisible, which made staleness untestable in practice.

**Consolidation** refuses to run while any tracked slot is generating — and a slot
whose state cannot be read counts as busy, because "cannot prove idle" is not "idle".
Work is one item per transaction, so interruption loses nothing partially written.

Summaries default to *extractive*: a deterministic selection of the episode's own
sentences, prefixing the identifiers it found. It is not a semantic compression and
does not pretend to be. That is what keeps the dual-track rule intact with no
auxiliary model resident — the original plan flagged that VRAM headroom question as open, so the
default assumes none. A configured auxiliary endpoint switches it to genuine
interpretation, still anchored.

A promotion must actually shrink the context. An extractive summary of a short or
dense turn can be *longer* than the turn, and an Atlas entry that costs more tokens
than the text it replaces is a regression dressed up as maintenance. The
`min_episode_tokens` threshold is a proxy; the measurement is the check.

**Receipts** measure, never estimate: the breakdown comes from the same `PromptParts`
that produced the prompt, counted with the same tokenizer, so FR-15's "category sum
matches the actual prompt token count" is a property of the code path rather than a
test that might drift. The cache verdict always carries its evidence — the boundary,
the checkpoint, how far the cut moved — because a status without its arithmetic is a
status nobody can act on.

---

## MCP gateway (C7, FR-14)

Targets spec 2026-07-28 on `rmcp` 3.3.0. List responses carry `ttlMs` and
`cacheScope`; the tool catalog is configuration-independent, so a long TTL with
private scope is correct and the bytes are identical between calls.

The tool split is deliberate: `code.*` returns parser output that cannot be wrong the
way a model is wrong; `memory.recall` returns stored memory with every interpretive hit
labelled; `context.plan_eviction` defaults to *planning only*, so an agent can see the
eviction decision before taking it.

The system preamble names *when* to call each tool rather than describing what they
do, because the original risk register notes that smaller local models under-trigger
proactive folding.

Integration tests drive the surface over a real HTTP listener with the SDK's own
client (`crates/sakur4d/tests/gateway.rs`), which is where JSON-RPC framing,
protocol negotiation and argument shapes get exercised. A handler-level unit test
skips all three.

---

## Storage and the dynamic backend layer

**SQLite in WAL, one writer connection behind a mutex, all work on blocking threads.**
WAL readers are independent, so the harness-facing `commit_episode` path is
non-blocking in practice (FR-1). Transactions are retried on `SQLITE_BUSY`.

**Vectors** live in a blob table with an exact cosine scan in Rust, unless a
`sqlite-vec` loadable extension is found — in which case `vec0` serves the ANN path
and the store records which backend is live. The original plan asked for `sqlite-vec`; linking a
C extension into a binary that must stay a single static artefact fights NFR-8, and a
*loadable* extension satisfies both. At the documented scale ceiling an exact scan is
a few tens of milliseconds and is exact, so recall quality never depends on which
backend is present. `doctor` prints which.

**The backend is probed, never assumed.** `InferenceBackend` has three
implementations — the llama.cpp HTTP adapter, an embedded simulation, and a null
backend for "cache coherence is off" — and `resolve()` picks between them from
`auto`/`embedded`/`none`/URL. `auto` probes and falls back to embedded, so a
developer with no server running still gets a fully functional Sakur4 whose cache
behaviour is *observably simulated*: the backend's name appears in the receipt.

---

## Token accounting

Three strategies behind one trait: exact counts from the backend's `/tokenize`,
a local BPE tokenizer if one is supplied, and a character-class heuristic
otherwise. The heuristic is calibrated for mixed prose/code/JSON and rounds up,
because under-counting would let a prompt silently overflow the window.

The rule that matters: **every** budget decision and every printed number goes through
one `TokenCounter`. Twice during development a component estimated while another
measured, and both times the engine decided "relaxed" while the receipt showed a
nearly-full window.

---

## What is not done

**This section listed completed work for several rounds.** The harness adapters, encryption at
rest, real-server verification and the NFR numbers were all done and all still listed here. That is
the more damaging direction for a document like this: a reader who checks it against the repository
finds it wrong in the direction of understating the project, and stops trusting the parts that are
right. Finished items have moved to [the requirement table](#requirements-and-where-each-is-met);
what follows is what is genuinely outstanding, each with its evidence.

* **FR-19's acceptance test** — the suite run with network egress blocked at the OS level. The
  default paths make no outbound calls and the live checks are opt-in behind `--upstream`, but the
  blocked-egress run itself has not been performed. The property is designed for; it is not
  demonstrated.
* **MCP reference-client conformance** — no run against the official conformance suite. Both
  transports are exercised against a real SDK client, which is not the same claim.
* **LoCoMo and the Endurance Benchmark** — not run. The A/B benchmark in [`bench/`](bench/) is a
  different and narrower measurement: matched windows, one repository, one model. It does not
  substitute for a standardised long-conversation benchmark.
* **FR-11's staleness annotation** — `impact_of_change` reports each caller's current signature but
  does not compute per-edge staleness, because edges do not record the target hash that was current
  when they were written. `ImpactEntry.stale` exists and is always `false`; it is the identified
  data-model change rather than a working feature, and it is left in place so the report shape does
  not change when it is implemented.
* **A live Hermes model session** — the engine's 44 contracts run against a live daemon, but driving
  Hermes with a real model has not succeeded: its provider routing rejects the model string this
  setup needs. Recorded in
  [`integrations/hermes-plugin/LIVE-TESTING.md`](../integrations/hermes-plugin/LIVE-TESTING.md).
* **Signed release artifacts** — the release publishes archives with SHA-256 checksums, which detect
  corruption and not tampering. There is no GPG signature and no build provenance attestation.
* **`cargo audit` / `cargo deny` in CI** — neither runs. A small, lockfile-pinned dependency set is
  not a substitute for a vulnerability feed.

### Deviations from the original plan, stated

* **`sqlite-vec` is optional rather than required**, with an exact-scan fallback
  (see above). The MCP contract is identical either way and the choice is observable.
* **`memory.recall` gained a fourth retriever** (Atlas BM25) beyond the three planned,
  because the Atlas was otherwise reachable only through the vector index.
* **Two tools beyond the planned list**: `context.plan_eviction` (inspect before
  committing — FR-15 implies the need but the tool was not named) and
  `memory.staleness`, plus `sakur4.status` and `sakur4.dream` for operability.
* **`EvictionPolicy` has knobs the plan did not name** (`cache_prefix_reserve_tokens`,
  `cache_prefix_max_tokens`, `keep_recent_tokens`, `max_prefix_ratio`). Without a
  prefix floor and a ceiling, "cache-aligned eviction" is not implementable: the
  boundary has to be chosen with both the cache and the eviction budget in view.
