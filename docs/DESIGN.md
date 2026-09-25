# Sakur4 design notes

This document records how each requirement is met, and — more usefully — the trade-offs and
corrections taken along the way. Requirements that are *not* met are listed at the end rather than
omitted.

## The Hermes check failed on a subtraction inside a log message

`context engine (FR-16)` failed with `2 contract(s) failed: something was evicted, compression_count
advanced` for many rounds, and it read as the engine refusing to compact. It was not. **The daemon was
panicking.**

Run against a 90-episode transcript, the daemon's own trace said:

```text
thread 'tokio-rt-worker' panicked at crates/sakur4-core/src/evict.rs:1332:17:
attempt to subtract with overflow
```

`Escalation::Proposed` formats `"N token(s) reclaimed"` from `tokens_before - tokens_after`. Two
blocks above it, the code documents that a `Masked` stub can be **longer** than the short episode it
replaces — that is why the ladder no longer refuses such a step, because a later rung does reclaim.
Both facts together meant the subtraction underflowed, and because it happened inside a `format!`
argument the panic landed on an async worker: the request was never answered.

The rest of the failure is downstream and looked like something else entirely:

* `context.plan_eviction` returned nothing, so the plugin's 20-second client timeout expired.
* `compress` treats a `None` plan as **"Sakur4 unreachable"** and returns the messages unchanged —
  the honest response to a daemon that did not answer, and precisely what the check observed.
* So a session's context was never compacted. Not on a short one, where the check's smaller probes
  pass, but on a long one — the only case where compaction matters.

**Fixed with `saturating_sub`.** The measurement that confirms it, against the same 90-episode store:

```text
plan answered: pressure=compacting live=11850 budget=8192 savings=4380 applied=true
updates: 43
NOTE: a step that reclaims nothing is present (5->37) — the case that used to panic
      its reason reads: tier live → masked (value 2.65, 0 token(s) reclaimed)
```

**On the regression test.** Three attempts at a unit test for this were removed rather than kept.
Each passed with the overflow restored, because the ladder needs enough pressure to propose a step
and the fixtures did not reach it — and a regression test that cannot fail is worse than none, which
is how this survived. What guards it now is `context engine (FR-16)` itself, which drives 44 contracts
against a live daemon and includes the transcript that reproduced the panic. That is recorded at the
fix site so the next person does not add the test back without checking that it fails first.

**Two nested wrong readings, both plausible.** The first was that the check never committed its
history, so the plan had nothing to evict — the engine already commits every message. The second was
that this was a stall rather than a crash, which is what a timeout looks like from outside. Neither
survived a look at the daemon's stderr, and both would have produced a "fix" somewhere other than the
bug.

## Projects are isolated

> **Fixed.** `episodic_stream` gained a `project_id` in migration 3; the MCP and CLI commit paths
> record it; episodic recall filters on it, closing the gap where only the Atlas retriever honoured
> the project a caller was working in; and `sakur4.status` reports `project_episodes` /
> `project_facts` / `project_atlas` scoped to the caller alongside the store-wide totals, plus
> `store_holds_other_projects` so a count that looks low is explained rather than surprising.
>
> `memory.recall` takes an optional `project_id` for a caller that wants to search another project
> deliberately; omitting it uses the daemon's own project, which is what keeps one project's
> transcript out of another's answers.
>
> Verified end to end: two project roots against one store, A commits, B recalls the same term and
> finds nothing while A finds its own turn; and the status counts agree.
>
> Rows written before migration 3 keep `NULL` and are **excluded** by a scoped query rather than
> attributed to whoever asks — an old episode's project is not recoverable, and guessing would
> reproduce the defect rather than fix it.

Reported from use, not from a test: working in OMP in one project surfaces material from another.
The cause was in the store's shape, and the notes below describe it as it was, because the shape of
the problem is what makes the fix legible.

**`episodic_stream` has no `project_id` column.** Its scoping keys are `session_id` and `slot_id`.
Everything else that matters does have one — `semantic_atlas`, `symbolic_fact`, `repo_file` and
`project` are all keyed by `project_id`, and `db.rs` filters on it in a dozen places — so episodic
memory is the exception rather than the rule, and it is the table the tools write on every turn.

What that produces, measured against one store with two project roots:

```text
A commits, with --project-root pointing at A
B asks with --project-root pointing at B
  B sees project_id: proj_cd0ca725b7a54d40   <- a different project, correctly detected
  B recall results: 0                        <- semantic recall IS isolated
  A recall results: 1
```

So the project *is* detected — `Engine::open` derives `proj_<hash(project_root)>` and semantic recall
honours it — and the leak is narrower and more specific than "everything is shared":

* `sakur4.status` counts are **global**. `DbStats` runs bare `SELECT COUNT(*)` against
  `episodic_stream`, `symbolic_fact`, `semantic_atlas` and `anchor_set` with no project predicate, so
  a session in project B is told how many episodes exist *across every project in the store*. One
  of those counts, `anchors`, is exactly what a caller would use to see what has been pinned for
  this work.
* **Episodic recall cannot be project-filtered at all**, because the column does not exist.
  `RecallFilters` carries a `project_id`, `search_semantic` applies it, and `recall.rs:557`
  post-filters semantic entries by it — but the episodic retriever has only a `session_id` filter, so
  a caller that does not pass one gets matches from every project.
* The `session_id` the OMP plugin derives is `omp-${basename(cwd)}`. Two projects whose directories
  share a name share a session id, and with no project column they share episodic memory too.

**What a fix requires**, recorded rather than attempted because it is a schema change and a tool
contract change at once: a `project_id` column on `episodic_stream` (migration, indexed, backfilled
from the owning project where derivable and left null where not), episodic recall filtering on it,
and `DbStats` taking a project so the counts describe the caller's work rather than the store's
contents. Until then the honest description is that Sakur4 keeps **one memory per store**, and the
project dimension exists for semantic memory but not for the transcript.

That is a worse position than the notes below imply, and it is the first thing to fix.

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
* **FR-11's staleness annotation is implemented** — migration 4 gave `dependency_graph_edge` a
  `target_hash`, the indexing pass records the caller's own hash when it first observes a call site,
  and `impact_of_change` compares it with the caller's hash today. The annotation is rendered by
  `ImpactReport::render`; it was computed and displayed nowhere before, which is a separate defect
  from the field being a constant.

  Verified on a two-function crate: a clean index shows no marker, changing the caller's signature
  and re-indexing shows `[STALE: changed since it last saw this symbol]`, and a further no-op
  re-index leaves it marked — because `insert_sql` deliberately does not refresh the column on
  conflict, since re-indexing the *target* is no evidence about the caller.

  The old note, kept because the reasoning still explains the design: *it reported each caller's
  current signature but did not compute per-edge staleness, because edges did not record the target
  hash that was current when they were written. `ImpactEntry.stale` existed and was always `false`;
  it was the identified data-model change rather than a working feature, and it was left in place so
  the report shape would not change when it was implemented.*
* **A live Hermes model session** — the engine's 44 contracts run against a live daemon, but driving
  Hermes with a real model has not succeeded: its provider routing rejects the model string this
  setup needs. Recorded in
  [`integrations/hermes-plugin/LIVE-TESTING.md`](../integrations/hermes-plugin/LIVE-TESTING.md).
* **Signed release artifacts** — the release publishes archives with SHA-256 checksums, which detect
  corruption and not tampering. There is no GPG signature and no build provenance attestation.
* **`cargo audit` runs in CI; `cargo deny` does not.** Running the audit by hand the first time found a
  **medium-severity vulnerability in `rustls`** — RUSTSEC-2026-0285, "TLS 1.3 handshake messages
  incorrectly accepted across encryption level boundaries", present in 0.23.44 and fixed in 0.23.45,
  reaching this project through `reqwest` for the reverse proxy. The advisory had been published ten
  days and nothing here would have said so, which is the argument for the step rather than for that
  particular fix. `cargo deny` remains unrun: it covers licences and duplicate versions, which is a
  policy question this project has not answered.
* **Three `PromptParts` slots are populated only by tests and the testkit.** `PromptParts` renders
  eight parts; every production caller uses five — `with_system`, `with_anchors`, `with_timeline`,
  and `with_recall` in two of the three. Nothing in `src/` calls `with_repo_map`,
  `with_tool_schemas` or `with_folds`; the only callers are `prompt.rs`'s own tests and
  `sakur4-testkit`'s harness.

  **This is less alarming than it first reads, and the first draft of this entry overstated it.**
  The one production assembler, `tools.rs::assemble_parts`, is a *diagnostic preview*: it exists so
  `context.plan_eviction` can show what a prompt would contain, and it is not the path any real
  request takes. Sakur4 never assembles the prompt a model receives — the harness does, and the
  proxy rewrites what the harness already built. Tool schemas and a repo map are the harness's to
  place, so their absence here is correct rather than missing.

  What is genuinely open is narrower:

  * `fold summaries` — `receipt.rs` accounts tokens to `RenderedPart::Folds`, so the receipt reports
    a category that is always zero, and a fold's summary reaches the model only as an episode via
    `unfold`. Whether it should also be a rendered part is a design question this document has not
    answered, and the token accounting implies an answer it does not implement.
  * `repo map` — `code.get_repo_map` exists as a tool and `RenderedPart::RepoMap` exists as a slot
    with a budget, and nothing connects them. The tool's own doc says a map "survives being
    compacted because it is regenerated", which presumes it is somewhere in the request.

  Recorded rather than changed: putting a repo map into the prompt is a product decision with a
  measurable cost, and the A/B benchmark is what would measure it. That is its own piece of work,
  not an edit at the end of an audit.

  **And there is a constraint on doing it that this entry did not state.** `assemble_parts` has four
  callers, and one of them is `context.plan_eviction` — which uses the result for **pressure
  measurement**, not for display:

  ```rust
  let parts = assemble_parts(&self.engine, &input.session_id, input.pending_recall.as_deref()).await?;
  let plan = self.engine.eviction().plan(…, &parts).await?;   // decides what to evict
  ```

  So populating `with_repo_map` there would make the engine plan against a prompt the client never
  sends — evicting sooner because of tokens that were never going to be in the request. The slots are
  unpopulated partly because the one assembler serves two purposes: a preview and a measurement. Wiring
  the map in is therefore not "call the builder"; it is **separating prompt assembly from pressure
  accounting**, with the plan measuring what the client will actually send.

  That is a smaller and better-specified piece of work than "add a repo map", and it is the reason this
  has stayed open rather than being a one-line change nobody got to.

  **And then it was tried, and the measurement is the argument.** `open_folds` was wired into
  `assemble_parts` — it returns `(fold_id, description, goal)` for every fold left open, and the preamble
  tells the model to *"call memory.unfold with fold_id"*, so an id that never reaches a request is an
  instruction that cannot be followed. The receipt afterwards read:

  ```text
  where the budget went:
    fold summaries            34   48.6%  █████████·········
    system prompt             22   31.4%  ██████············
    raw recent history        14   20.0%  ████··············
  ```

  **`parts.render()` is never transmitted to a model.** Its only consumers are token measurements —
  `PromptParts::total_tokens`, `Receipt::build`, and `CacheCoherence::observe_prompt`. So the change added
  48.6% to the **pressure** that `plan_eviction` evicts against, for text that still reached nobody, and
  the model's inability to see a fold id was exactly as before. It was reverted, and the reasoning is in
  the function.

  This is the clearest statement of why the three slots are empty: **a slot that is counted but not sent
  is worse than a slot that is empty**, because empty costs nothing and counted-but-unsent makes the
  engine evict earlier for content no client was ever going to receive. Filling them requires the
  separation below to exist first, not a call to a builder.
* **Nothing ever marks an episode superseded — and now something does.** The eviction engine's scoring
  reads `episode_row.superseded_by` and subtracts 3.0 from an episode's value when it is set, with the
  note *"superseded by a later episode"* — built to drop a stale copy before its replacement. The only
  writer of that column is `MemoryFabric::mark_superseded`, and **nothing called it**: the field, the
  column, the `EdgeKind::Supersedes` variant and the scoring rule all existed, and no code path produced
  the value they act on.

  **Fixed.** `memory.commit_episode` takes an optional `corrects`, which names the episode this turn
  replaces; the handler calls `mark_superseded` after the commit (a separate transaction, so a
  correction that fails to record cannot lose the turn that was already accepted), and the response
  echoes `supersedes` back so a caller can confirm the reference landed. A correction whose id matches
  nothing is a `NotFound` error rather than a silent success — the first version ignored the update's
  row count and reported success for an episode that did not exist.

  So the rule is inert. An episode that a later one has replaced looks exactly like one that has
  not, and the eviction engine pays the cost of a comparison that can never be true. The two
  variants that would express the same relation — `Corrects` and `FoldedFrom` — are declared for
  the same purpose and are likewise never constructed.

  Not deleted, and not wired up here. Deleting `mark_superseded` would remove the only statement of
  intent for a relation the scoring code is written against; wiring it up needs a decision about
  *who* decides an episode is superseded — the agent, a re-read of the same file, or a fold — and
  that is a product question, not a cleanup. Recorded so the next reader finds it in the list of
  things known to be missing rather than discovering that a scoring rule never fires.

  Worth naming how this entry was written: the audit found "no production caller" and I recorded it
  as a wiring gap. Reading the assembler showed it is a preview path, and the claim had to be
  narrowed. The first version was true about the call sites and wrong about what they mean.
* **A read can observe the store before a pipelined write in front of it has landed.** Cause
  established; the fix is not written.

  **The trigger is pipelining.** Writing every request at once and closing stdin makes
  `sakur4.status` report zero counts for rows that exist. Sending the same requests one at a time,
  waiting for each answer, reports them correctly:

  ```sh
  # pipelined — all frames written, then stdin closed
  memory.commit_episode {content: "hello"}  -> ep_01a0d1a83914752e8684d5957ea7a0a6
  sakur4.status {}                          -> episodes 0
  sqlite> SELECT COUNT(*) FROM episodic_stream  -> 1

  # sequenced — status sent only after the commit's answer arrived
  sakur4.status {}                          -> episodes 1
  ```

  **The write reports success either way**, returning a real `ep_…` identifier from a store the next
  request cannot see, and the daemon's own trace has the two answers 0.1 ms apart:

  ```text
  17.945350  response id=2  commit -> ep_01a0d1a8...
  17.945443  response id=3  status -> episodes 0
  ```

  **The boundary, narrowed to one sentence: a read does not observe a write from the same pipelined
  batch, and observes any other write correctly.** Two experiments settled that, and they are the
  ones worth repeating if this is picked up again:

  | experiment | result |
  |---|---|
  | a daemon session against a store written by an **earlier process**, asking only for status | **`episodes 1`** — correct |
  | commit and status **in the same batch** | **`episodes 0`** — wrong |
  | the same two sent one at a time | `episodes 1` — correct |

  So `db.stats()` reads the store faithfully, the store layer is sound, and the write commits — a
  later process opens the same file and sees the row. What fails is visibility of a write to a read
  pipelined behind it in the same session.

  **It is in the released binary**, not introduced later: `v0.1.0` from `~/.cargo/bin` reproduced it, and `v0.2.0` still does
  and does not have the `open_folds` field, so it predates that change.

  This is the defect that `context engine (FR-16)` has been failing on. Those contracts commit
  episodes and then read back what the daemon reports, and a check that pipelines them sees the
  stale answer — which is why the failure looked like eviction being broken and why it appeared in
  CI while passing locally, where the harness happened to sequence the calls.

  **Why nothing caught it earlier.** Every gateway test awaits each call before making the next, so
  the test suite structurally cannot reach this. `status_counts_what_was_written_to_a_file_store`
  was written to reproduce it and does not, for that reason; it is kept as the control that rules
  out the store, the `Db` clone, the tool router and the resolved path.

  **A concurrency fix was tried, and it did not work.** Serialising the MCP tool surface — a
  `tokio::sync::Mutex` held across `call_tool`'s dispatch, so one tool completes before the next
  begins — changed the pipelined result by nothing at all: still `episodes 0`. It was reverted rather
  than kept as a plausible-looking change.

  **That is the most useful thing this round produced**, because it removes request ordering as the
  mechanism. The read is not racing the write: the commit has answered, the store holds the row, a
  later session sees it, and the status in the same session does not, with the requests processed in
  order. What remains is that `sakur4.status` reads something the others do not — and the candidates
  are narrower than they were.

  **Instrumented from inside the store, which is the evidence that settles it.** A temporary
  `tracing::info!` in `Db::stats()` — reporting both the `scalar()` result and a direct
  `query_row` — was run against both cases and then removed:

  | case | response | store on disk | `stats()` saw |
  |---|---|---|---|
  | pipelined | `episodes 0` | 1 | `scalar=0, direct=Ok(0)` |
  | sequential | `episodes 1` | 1 | `scalar=1, direct=Ok(1)` |

  So the discrepancy is **inside the store layer, on the connection**, and not in the mapping from
  `DbStats` to the response: the connection genuinely does not hold the row when the read runs in the
  pipelined case, and does in the sequential one. `episodic_stream` is a real table, not a view, and
  both `Db::with` and `Db::write` lock the same `Arc<Mutex<Connection>>`, so a committed write on
  that connection would be visible to the next reader of it.

  **The write is acknowledged before it becomes visible, and it does land.** One session, three
  requests:

  ```text
  commit answered          : ep_01a0d1b97abb71a197651ec6be3
  status, pipelined behind : 0
  status, 1.5s later       : 1     <- the same session, the same connection
  ```

  So this is neither a lost write nor a stale-snapshot read: the row arrives, and a read that comes
  after it does see it. What is wrong is the **ordering of the acknowledgement** — the tool returns
  before the write is observable to the next reader.

  **The lag is structural, not a settling delay.** Repeating the pipelined pair inside one session
  shows a read that is exactly one write behind, not a read that catches up:

  ```text
  round 1  commit "one", status pipelined behind it   -> 0     (expected 1)
  round 2  commit "two", status pipelined behind it   -> 2     (expected 2, and it is right by luck)
  round 3  status alone                               -> 2
  ```

  Round 2 reports the count from *before* its own commit — two episodes, both of which existed when
  it ran — so the read is not late, it is answering an earlier question. A read sees every write
  except the one immediately in front of it.

  **This is NOT a durability defect, and an earlier draft of this entry said it might be.** Killing
  the daemon with `SIGKILL` the instant a commit's answer arrives leaves the episode in the store —
  measured: nineteen commits written as a pipelined batch, killed hard, nineteen rows present. So
  `Db::write` commits durably and NFR-5/NFR-6 hold. The scope is visibility, which is narrower and
  less alarming than the draft claimed, and the correction matters because a durability scare would
  send the next reader to the wrong place.

  **And the write is awaited**, which is what makes this hard to place. `commit_episode` closes its
  transaction at `fabric.rs:191` with `.await?` and only then builds `CommitOutcome` at 201, so the
  handler's future completes *after* the transaction. The await is correct, the transaction commits,
  the row lands, and the acknowledgement still precedes visibility — so what remains is inside
  `Db::write`'s `spawn_blocking` and the runtime it runs on, none of which is visible from outside
  the process.

  **Where the next attempt should start.** The fix is not in the MCP layer: serialising dispatch was
  tried and changed nothing. It is in `Db::write` — something about how its `spawn_blocking` task
  relates to the caller's await — and the first measurable step is to time a batch of commits. If
  they serialise, the await is honest and only *visibility* lags; if they all return immediately, the
  await is not waiting for the closure and that is the defect.

  **The measurement, taken, and it answers the question.** Twenty commits written as one pipelined
  batch, arrival offsets in milliseconds:

  ```text
  11 8 7 6 9 14 9 16 13 14 10 11 15 17 12 19 17 20 20 18
  first=11ms  last=18ms  span=7ms
  ```

  Twenty commits answered inside a 7 ms window, which is not twenty serialised SQLite transactions —
  and it is not consistent with `Db::write`'s `.await` blocking each caller until its own closure
  returns either, since the closure cannot run twenty times in 7 ms while the mutex serialises it.

  **So the write path is not the problem, and neither is the await.** What the timings show is that
  the MCP server dispatches requests as they arrive and completes them concurrently, so a read sent
  *after* a write can still be executed *before* it. The client's ordering is not the server's
  ordering, and nothing in this project makes a read wait for a write it was pipelined behind.

  **That also explains why the MCP-layer mutex did not help**, which had been the loose end: it was
  applied to `Sakur4Server`'s `call_tool`, and it should have ordered the two handlers. Either it did
  not span the dispatch as intended, or the writes complete outside the handler's own future. Both
  are checkable now, and both are in the same place — which is much narrower than the seven rounds
  of search that preceded it.

  **The fix belongs in the store, not the transport.** A read should wait for any write already in
  flight on the same `Db`, so that a client which pipelines a write and a read gets the ordering it
  asked for regardless of how the transport schedules them. `Db` already holds the mutex both paths
  need; what is missing is that a read does not take it in a way that observes a write's completion.

  **That fix was written, and it did not work either.** `Db` gained an `Arc<tokio::sync::Mutex<()>>`
  held across the whole of both `write` and `with` — a lock the runtime can schedule, unlike the
  `parking_lot` one that lives inside `spawn_blocking` — precisely so a read would queue behind an
  in-flight write. The pipelined probe still reports `episodes 0`. The change was reverted; the suite
  passed with it (257 tests, no deadlock), so it is not obviously wrong, but an unproven change in
  the store's critical path is worse than none.

  **So two fixes have now failed** — serialising the MCP tool surface, and serialising store
  operations — which rules out a third family of explanations: the ordering of the two calls is not
  what is wrong, at any layer that has been tried.

  **The trace, in full order, and it is the answer.** Requests are received 0.09 ms apart and the
  responses are logged 0.12 ms apart:

  ```text
  11.374180  received request id=2  memory.commit_episode
  11.374269  received request id=3  sakur4.status
  11.378768  response message id=2  {"episode_id":"ep_01a0d1cf7d8e76f796a08015b6acea39", …}
  11.378889  response message id=3  {"anchors":0, … "episodes":0, …}
  ```

  The commit's handler took **4.6 ms** to answer; the status handler answered **0.12 ms** after it.
  A read that had waited for that transaction would have taken a comparable time; one that took a
  tenth of a millisecond did not wait for anything. (An earlier round read this trace as "no
  response line for the status" — that was wrong, the line is there, and the two timestamps are the
  part that matters.)

  **And it is not every read.** Pipelining a commit, a `memory.recall` and a `status` together:
  recall finds the episode, status reports zero. Recall does more work — BM25 over the transcript,
  a vector scan, a rerank — and outlasts the commit by accident. **The difference is duration, not
  the read path.**

  **So the write completes after its handler has returned**, which is why serialising callers
  changed nothing and serialising store operations changed nothing: both were ordering things that
  had already finished. `Db::write` builds a transaction, calls `tx.commit()` inside a
  `spawn_blocking` closure, and awaits that task — and the closure's work is still observably
  completing afterwards, on the blocking pool, while the handler that awaited it answers.

  **Where that leaves the fix.** Not in the transport, not in an operation lock, but in how the write
  reaches the connection: the write has to be complete before `Db::write` returns, on a path whose
  completion the caller actually waits for.

  **That change was made, and it moved the bug without closing it.** `Db::write` now runs its
  transaction on the calling task — no `spawn_blocking` — so the lock is taken, the closure runs and
  `tx.commit()` returns before `write` does. Dispatch was then serialised as well, so two pipelined
  requests are served in arrival order rather than in whatever order they reach the store. Both
  changes are in, the suite passes (257 tests), and the behaviour changed in a way worth recording
  precisely, because it is not fixed:

  | probe | before | after |
  |---|---|---|
  | one commit + one status, batched, 8 runs | 0 in 8/8 | **0 in 8/8** |
  | a **second** status in the same batch | — | **1, reliably** |

  So the first read after a commit still misses it, and the *next* read sees it — which is the same
  "one write behind" shape, now with the write and the reads strictly ordered and the write provably
  finished (its handler returns only after `tx.commit()`). The remaining possibility is that
  `tx.commit()` returning is not the same as the row being visible to the next reader *on the same
  connection*, which is a statement about this store's pragmas rather than about scheduling.

  **Two more fixes ruled out, then**: ordering the callers, and ordering the store's operations. Both
  were correct about ordering and neither was the problem. What is left is a single question — what
  makes a committed row visible to the next statement on the same `Connection` — and it is now
  narrow enough to answer by reading the pragmas `Db::open` sets rather than by probing from outside.

  **The pragmas were read, and they clear the store.** A fresh store reports
  `journal_mode=wal`, `synchronous=2` (FULL), `locking_mode=normal`, `read_uncommitted=0`. In WAL a
  committed transaction is immediately visible to later readers on the same connection, and
  `read_uncommitted=0` is the default, so nothing here explains a committed row being invisible.

  **And there is only one connection.** `Engine::open` calls `Db::open` once (`engine.rs:165`) and
  every component shares it — `MemoryFabric::new(db.clone())`, `Coherence::new(db.clone())` — and
  `Db::with` and `Db::write` both clone the same `Arc<Mutex<Connection>>`. The full list of
  `Db::open*` call sites in `crates/` contains exactly one non-test, non-encrypted opening. So the
  read and the write cannot be on different connections, and the "it reads a different store"
  family is closed for good.

  **Where that leaves it.** Twelve rounds have produced: the trigger (pipelining), the boundary (a
  read misses the write immediately in front of it), durability cleared, four ruled-out fixes, and
  the store itself cleared. The remaining explanation has to be that the commit is not reaching
  `Db::write` at the time it appears to — that the tool's answer is produced on a path that does not
  include the write, despite `commit_episode` ending in `.write(…).await?` at `fabric.rs:191`.

  **The order was then measured, and it is the answer.** With a temporary probe at `call_tool`'s
  entry, after it took the serialising lock, at its exit, and at `Db::write`'s entry:

  ```text
  +   0ms  call_tool ENTER  sakur4.status
  +   0ms  call_tool ENTER  memory.commit_episode
  +   0ms  call_tool LOCKED sakur4.status          <- the READ takes the lock first
  +   2ms  call_tool EXIT   sakur4.status
  +   2ms  call_tool LOCKED memory.commit_episode
  +   2ms  Db::write ENTER                          <- the write has not even started
  +   6ms  call_tool EXIT   memory.commit_episode
  ```

  **The status handler runs to completion, including its read, before the commit's transaction is
  entered.** The write is four milliseconds in the future when the read returns zero — and there is
  nothing wrong with either of them. `Db::write` is synchronous and correct; the read is correct; the
  store is correct. **The requests are simply executed in an order the client did not ask for**, and
  the serialising lock does not fix that because it orders handlers by whichever reaches it first,
  which is not arrival order once `rmcp` has dispatched them concurrently.

  **So the defect is one sentence after twelve rounds:** a client that pipelines a write and a read
  gets them executed in an arbitrary order, because request dispatch is concurrent and nothing
  restores the order the client sent. Every symptom in this entry follows from that — the read one
  write behind, the reproducibility only when batching, `memory.recall` passing where `status` failed
  (recall takes long enough to lose the race later), and the failure appearing in CI while passing
  locally where the harness happened to sequence its calls.

  **The fix is therefore in dispatch, and it is not a lock.** Ordering has to be imposed where
  requests arrive — reading them in sequence and completing each before reading the next, or
  otherwise making the service honour arrival order — rather than by synchronising handlers that have
  already been started. Two locks were tried and neither could work for that reason.

  **A serialising transport was tried, for the second time, and improved this by 1/6.** `serve_stdio`
  was rewired to feed the server through a bounded pair of one-way channels — requests in, responses
  out — so stdin is read only as fast as the server drains messages. Measured over six runs of the
  same batched probe:

  ```text
  before (direct stdio):   0/8 runs correct
  after  (serialised):     1/6 runs correct
  ```

  The first attempt at this, two rounds earlier, answered *nothing* — it built a one-way pipe and gave
  the server nowhere to write its replies. That was a plumbing bug, and fixing it produced a real but
  unreliable improvement rather than a fix. **Reverted**: a marginal gain that I cannot reproduce
  reliably is not worth rewiring the transport that every stdio harness depends on.

  **What that rules out.** Reading messages one at a time is not sufficient, because the server reads
  ahead and then *dispatches* what it has read concurrently. Serialising delivery does not serialise
  execution — the dispatch happens after the read, so a bounded buffer changes how fast messages
  arrive and not the order they run in.

  **The dispatch lock is already in place and does not help either.** `Sakur4Server::call_tool` holds a
  `tokio::sync::Mutex` across the whole handler, and `Db::write` completes inside its own handler since
  round 31 — so the two conditions that should have made handler ordering sufficient are both met, and
  the probe still reports the pre-write count most of the time. That is worth stating plainly: the
  evidence contradicts the model of the failure I have been reasoning from, which is why five fixes
  have failed.

  **What is left, and it is a narrower question than any asked so far.** The commit's response is
  produced somewhere other than the code path the lock guards — `commit_episode` mints its `ep_…`
  identifier *before* the transaction and returns it with the outcome, so a returned identifier is not
  evidence that a commit landed, and the answer may be emitted from a task that never took the lock.
  Instrumenting `call_tool`'s entry, the lock acquisition and `Db::write`'s entry *together* — as was
  done to find the original trace — would settle whether the two handlers ever overlap at all.

  **Instrumented, and the answer is that the server dispatches in an arbitrary order.** Six probes —
  `call_tool`'s entry, the dispatch lock, `Db::with`'s entry and lock, `Db::write`'s entry and lock —
  each recording a thread id, on a batched commit-then-status:

  ```text
  +0ms  ENTER  sakur4.status            thread=ThreadId(9)
  +0ms  LOCKED sakur4.status            thread=ThreadId(9)   <- dispatch lock, held by the READ
  +0ms  ENTER  memory.commit_episode    thread=ThreadId(6)   <- the commit has not started work
  +0ms  Db::with ENTER  (status reads)
  +5ms  LOCKED memory.commit_episode    thread=ThreadId(6)   <- only after status finishes
  +5ms  Db::write ENTER / LOCKED
  ```

  **The dispatch lock works exactly as intended.** It serialises the two handlers — the commit cannot
  take it until the status has released it. What it cannot do is decide *which handler starts first*,
  and the server begins the read before the commit's handler has done anything.

  **The execution order is arbitrary, and varies run to run.** Three commits sent in the order
  `0,1,2`, recording the `seq` each was assigned, over four runs:

  ```text
  2,0,1  |  2,0,1  |  2,0,1  |  0,2,1
  ```

  Five calls, over four runs:

  ```text
  0,1,2,4,3  |  0,1,4,2,3  |  0,1,2,4,3  |  4,0,2,1,3
  ```

  Not LIFO, not FIFO, not a stable permutation — **arbitrary**. That single fact explains the whole
  entry: why the failure is probabilistic rather than deterministic, why nineteen commits in one batch
  all survived a `SIGKILL` (they mostly run in order), why `memory.recall` passed where
  `sakur4.status` failed (recall does more work, so it loses the race less often), and why CI
  reproduced it while a hand-typed session did not.

  **So the fix is not a lock, and this is the last thing that was missing.** Serialising handlers
  cannot impose an order on handlers the server has already started in its own order — which is what
  five failed fixes all assumed, in one form or another. Order has to be imposed *before* dispatch: a
  request must not be handed to the server until the previous one has been answered.

  That is a bounded, well-specified change now: a queue at the receive point that holds request N+1
  until request N's response has been emitted, which is what the two transport attempts were reaching
  for without this constraint to tell them when to release the next message.

  **A sixth fix was attempted and reverted — the third at the transport.** It built the right thing and
  wired it wrong: a `turnstile` semaphore with one permit, held by the pump while a request is in
  flight and released by the handler when it finishes, so the server is never handed message N+1 until
  N has been answered. The control flow was correct and the plumbing was not — the server stopped
  answering entirely, which is strictly worse than the defect it was meant to fix, so it did not ship.

  Three things were learned that the next attempt should start from, and none of them is a guess:

  * **The pump must read freely and gate the *write*.** Gating the read deadlocks against exactly the
    client shape that reproduces this: one that writes its whole batch and closes stdin.
  * **`tokio::io::split` on a `duplex` cannot be used to give the server one half and the pump the
    other.** A duplex is one bidirectional channel; splitting it and then also reading the peer end
    gives two readers on one buffer, and the inbound/outbound split that looks obvious does not work.
    The server needs a genuine read side and write side, which means `tokio::io::simplex` or a stream
    wrapper rather than a split duplex.
  * **The permit has to be released by a guard**, not at the end of the handler. `call_tool` returns
    from the middle on a panic and has two exit paths; a permit that is not released stops the
    transport reading anything else, and the session hangs rather than fails.

  Recorded because all three are mistakes about plumbing rather than about the idea, and the idea is
  now measured and sound. Six fixes have failed; the last one failed by wiring, having for the first
  time had the right constraint to satisfy.

  **A seventh attempt did the plumbing differently and still moved the number by chance.** The
  transport was rebuilt on the verified two-channel shape — requests in on one channel, responses out
  on another, which
  `a_relay_carries_requests_in_and_answers_out` checks on its own — and the turnstile was added on top:

  ```text
  batched pair, 10 runs:  0/1  0/1  0/1  1/1  1/1  1/1  1/1  0/1  0/1  0/1     -> 4/10
  five commits sent 0…4:  0,2,4,1,3  |  0,1,2,4,3  |  0,4,2,3,1  |  1,0,2,4,3  |  1,0,4,3,2
  ```

  **Two permits, so two messages can be in flight.** The pump takes one before writing message N and
  drops it as soon as the write completes, while the handler takes its own for the duration of the
  work. Between those two windows the next line can be written, so the gate admits a second request
  and the ordering is arbitrary again. The improvement from 0/8 to 4/10 is that race landing the right
  way half the time, not a fix — and it was reverted for the same reason the previous one was.

  **What the next attempt needs, stated exactly.** One permit, held **across the response**, not
  across the write: the pump must not release until the handler for its message has finished, which
  means the pump and the handler cannot each take their own. Either the pump takes the single permit
  and the handler signals completion back to it, or the response is written to the client by the same
  task that read the request. Both are small, and both are a different shape from what has been tried.

  The two transport tests added along the way are kept. They check the plumbing in isolation and are
  why this attempt is known to have failed in the gate rather than in the channels — which is the most
  that can be said for eight failed fixes, and it is more than could be said for the first six.

  **A ninth attempt did not write a gate at all — it wrote the test that says what "fixed" means.**

  The defect has been disclosed in the README for many rounds without a machine that can disagree, which
  is how a project ends up with a limitation nobody ever closes. So a test was written first, against the
  real binary over real stdio, and it **fails**:

  ```text
  a read in a batch was served before the write in front of it — 1/12 rounds wrong:
  status 101 reported 0, but 1 commits preceded it
  ```

  It sends a commit and a status **twelve times in one session, all frames written before any is read**,
  and requires every round to be correct. A single round would have passed about half the time — the
  failure is probabilistic — so twelve is what makes it a test rather than a coin flip, and the reported
  fraction is what will show the fix working.

  It is **not committed**, because a suite that fails is a suite people stop reading, and CI runs on every
  push. What is committed is this record. The test lives in the elimination history below and is restored
  by pasting it into `crates/sakur4d/tests/stdio_transport.rs`:

  ```rust
  #[tokio::test]
  async fn a_read_in_a_batch_sees_the_write_before_it() {
      // Spawns `sakur4d ... serve --transport stdio` with piped stdin/stdout, writes
      // `initialize`, `notifications/initialized`, then ROUNDS pairs of
      // (`memory.commit_episode`, `sakur4.status`) — all of them before reading a line —
      // closes stdin, collects the status reports, and asserts that a status sent after
      // N commits reports at least N. Twelve rounds, because one would pass by luck.
  }
  ```

  **Whoever writes the tenth fix should add that test first and watch it fail**, then implement, then
  watch the fraction fall to `0/12`. That sequence is the thing this bug has never had.

  **A tenth attempt took that advice and got further than any before it, then was reverted.** It put the
  gate at the **transport write** rather than in a handler, which is the first design here that does not
  need a handler's cooperation or a second permit:

  * The server is given a genuine read channel and write channel, not one duplex split in two.
  * The pump forwards a message and then waits for the **response to it to be flushed to the client**.
  * A small `AsyncWrite` wrapper sets a release flag on every `poll_flush`.

  Two things about it are worth keeping. First, **the reasoning for gating on flush rather than on a
  handler signal**: every request produces a response, so the release happens whatever the method was,
  which makes the gate method-agnostic and impossible to deadlock — where a gate waiting on a signal only
  `call_tool` sends would hang on `initialize`, `tools/list`, or any notification. Second, **the first
  real bug it hit, found by running the test rather than by reading code**:

  > `notifications/initialized` has no `id` and produces no response, so it must not arm the gate.
  > It is the second frame of every MCP session, and the first version blocked on the line after it and
  > never forwarded another message.

  That was fixed and the gate still did not work: **one reply out of twenty-five, then a stall**. A
  one-slot `mpsc` with `try_send` **silently drops a release when the slot is full**, so a later request's
  release was lost and the pump waited on a message that had already been discarded. Replacing it with an
  `AtomicBool`, which cannot fill up, moved the failure to a different stall rather than fixing it, and at
  that point the attempt was reverted — the daemon answered 1 of 25 requests, which is worse than the
  defect it was meant to repair, and the rule this project has settled on is that a broken daemon is not a
  candidate for a long debugging session at the end of a round.

  **What the eleventh attempt should know, and none of this is inference:** the transport-write gate is
  the right *place*; the notification case is real and must be handled; a release mechanism that can drop
  a signal is worse than useless; and the reply count (25 of 25) is the control — a correct gate must
  still answer every request, because the disclosed defect reorders answers and never omits them.

  **An eleventh attempt found the second lost-release bug by reading `tokio`'s source rather than by
  guessing, and still stalled.**

  `tokio::io::copy` does **not** flush per message. From `tokio-1.53.1/src/io/util/copy.rs`:

  ```rust
  Poll::Pending => {
      // Ignore pending reads when our buffer is not empty, because we can try to write data
      // immediately.
      if self.pos == self.cap {
          // Try flushing when the reader has no progress to avoid deadlock
          if self.need_flush {
              ready!(writer.as_mut().poll_flush(cx))?;
  ```

  It sets `need_flush = true` after a write and flushes only when a read comes back `Pending` with a full
  buffer. So **counting flushes counts the wrong thing**, and any gate built on `poll_flush` fires at a
  moment determined by the reader's behaviour rather than by a response being complete.

  That also named the `AtomicBool` failure exactly: `swap(false)` **clears** the flag, so a store landing
  in the window between the writer's store and the reader's next check is erased — the same lost-release
  class as `try_send` on a full channel, which is why the flag "moved the stall rather than fixing it".

  The fix attempted was a **monotonic counter** — `AtomicUsize`, incremented once per newline accepted by
  the transport writer, compared and never cleared by the pump, with a 120-second release so a stall
  becomes an error rather than a hang. That is the right shape for a release mechanism, and it is recorded
  because it removes a whole class of failure rather than a symptom.

  **It still produced 0 replies of 25**, and was reverted. So the counter is not sufficient, which means
  the next attempt should question the layering rather than the release mechanism: the transport is
  hand-wired as two one-way duplexes plus a pump plus a counting writer, and nothing in that arrangement
  has ever been shown to deliver a single response end to end. The relay test proves the two-channel shape
  in isolation; **nothing proves the pump in front of it**, and that is the smallest unproven thing.

  **A twelfth attempt proved the pump, and found the constraint that had been missing all along.**

  The pump was proved first, in isolation, with no gate attached —
  `a_pump_in_front_of_two_channels_delivers_one_response`, which passes and is kept. Then the same shell
  was put in the real daemon with **nothing held back**, only to measure it:

  ```text
  replies 25/25   statuses 12/12   WRONG 0, 3, 2 across three runs
  ```

  **25 replies of 25** — the control that says the shell is sound — and the wrongness is exactly the
  disclosed defect, out of order and never omitted. That is the first time any attempt has reached this
  point with an intact daemon.

  **And it was still reverted, because it truncated large responses.**

  ```text
  initialize + notifications/initialized + tools/list   ->  replies 1        (id 2 missing)
  initialize + notifications/initialized + sakur4.status ->  replies 1,2
  ```

  The difference is size: `tools/list` returns seventeen tools with their schemas, `sakur4.status` returns
  a few hundred bytes. With stdin at end-of-file, which is how every batch client terminates, the pump
  calls `shutdown()` on the server's read side — and **the existing `stdio()` transport evidently drains
  in-flight responses at EOF where this does not**, so the large write is cut short.

  **That is the constraint eleven attempts were missing, and it is not about ordering at all.** Any
  replacement transport must preserve `stdio()`'s end-of-file behaviour: EOF ends *input*, it does not
  abandon output already being produced. A gate layered on a transport that loses the last response would
  fix ordering by breaking delivery.

  Reverted, with the control restored — `replies 1,2 · tools 17`.

  **What the thirteenth attempt should do, in order:**

  1. Prove the shell answers 25 of 25 **and** that `tools/list` returns seventeen tools, with nothing held
     back. Both, because the first is what the twelfth attempt measured and the second is what it broke.
  2. Only then add the gate, re-running both.

  **A thirteenth attempt did step 1 and identified the truncated-response cause by isolating it, not by
  guessing.**

  A pump-only transport with a **1.5-second delay before the read side closed** produced both:

  ```text
  control 1  tools/list -> 17 tools                 (was 0)
  control 2  replies 25/25   statuses 12/12         three runs, WRONG 4, 3, 1
  ```

  **So it was timing, and it is now proved rather than inferred.** `yield_now()` between messages was not
  enough; a delay was. The read side closing as soon as stdin ended cut a write in progress, and the reason
  `sakur4.status` survived while `tools/list` did not is response size.

  With both controls satisfied, the real mechanism was put in its place of the delay — a shared
  `AtomicUsize` of completed responses, incremented by a writer that counts **newlines** rather than flushes
  (because `tokio::io::copy` flushes only when the reader stalls, so counting flushes counts the wrong
  event), and read by the pump for two purposes: the ordering gate before forwarding message N+1, and the
  end-of-file drain before closing the read side.

  **A counter rather than a flag or a channel, because both of those lose a signal.** An `AtomicBool`
  cleared with `swap(false)` erases a store landing between the writer's store and the reader's next check;
  a one-slot `mpsc` with `try_send` drops a release while the slot is full. Every increment of a counter is
  observable and nothing is cleared, so neither can happen.

  **It produced 0 replies of 25**, and was reverted — so the counter alone is not sufficient either. What
  remains unexplained is narrow and worth stating exactly: a shell proven to deliver 25 answers and a
  17-tool catalog **with a delay**, plus a release mechanism that cannot lose an event, still stalls when
  the two are combined. The next attempt should instrument the counter itself — print it from both sides —
  rather than reason about it, because every previous attempt reasoned and twelve were wrong.

  **The protocol says the reordering is legal, which reframes the whole entry.** From the JSON-RPC
  2.0 specification:

  > The Server MAY process a batch rpc call as a set of concurrent tasks, processing them in any
  > order and with any width of parallelism. The Response objects being returned from a batch call
  > MAY be returned in any order within the Array. The Client SHOULD match contexts between the set
  > of Request objects and the resulting set of Response objects based on the `id` member.

  And the MCP 2026-07-28 stdio transport says the same thing structurally — responses are
  "correlated by JSON-RPC `id`" — and adds that "MCP has no protocol-level session, so a server
  cannot rely on implicit per-connection state to relate one tool call to the next."

  So Sakur4 is not being reordered *against* the protocol; it is being reordered *with* it. What is
  wrong is that its tools are stateful while its transport is not ordered, and the preamble it ships
  tells the model to rely on exactly the ordering the protocol declines to provide: commit every
  turn, then consult what you remember. **A conformance-minded server is free to do what this one
  did, and an agent that batches is broken by it.**

  That reframing matters for the fix as much as for the diagnosis. Serialising the transport would
  make this server ordered, which the spec permits and which no client can rely on — so the durable
  answer is probably not a serving-model change at all, but making the *tools* safe under reordering:
  a read that observes a write it was pipelined behind, or returns something the caller can detect as
  early, rather than silently answering from the past. Recording that as the direction rather than
  attempting it here: it is a design decision about the tool contract, and it deserves to be made
  deliberately instead of at the end of a long search.

  **A serialising transport was attempted and reverted.** Routing `serve_stdio` through a
  `tokio::io::duplex` pair that the server drains one message at a time looked like the smallest
  change that imposes arrival order; it compiled and then answered nothing at all, because `duplex`
  is not a transport `rmcp::serve_server` accepts. It was reverted rather than debugged, and the
  lesson is worth keeping: **seven fixes have now been attempted for this bug and all seven were
  reverted** — four locks, a synchronous write, and now a transport change. The one thing that has
  never needed reverting is a measurement.

  **The probe was temporary and is removed**, along with a round-31 change that is no longer doing
  work: `Db::write` still runs its transaction on the calling task rather than in `spawn_blocking`,
  which the timings show is not what was wrong. It is left in place only because it is correct and
  tested; the ordering fix will not depend on it.

  **The loose end from two rounds ago now has a shape.** Serialising MCP dispatch should have ordered
  the two handlers and did not, which fits an acknowledgement produced outside the handler's own
  await chain rather than a race between handlers.

  **Why this matters beyond one status field.** `commit_episode` is FR-1's only write path, and the
  preamble tells the model to commit every turn. An agent that commits and then asks what it
  remembers is told nothing — and every read it makes for the rest of that turn is one commit stale.
  For a memory layer whose stated contract is "the stable surface is the tool result", a tool result
  that is always one write behind is a contract that does not hold.

  It is **not** a durability problem — that was measured and cleared above — so the honest severity
  is: the data is safe, and what the tools say about it is late.

  **A caution for the next attempt.** `rmcp` dispatches requests concurrently and this project does
  not control that, so a fix reasoned from "the calls must have interleaved" has to be *tested*
  against a pipelined probe rather than argued. Four rounds of reasoning have produced four wrong
  causes; the experiments that produced real information compared paths against each other or read
  the store from inside, and the ones that produced nothing reasoned about the framework.

  Ruled out along the way, so the next attempt need not repeat it: `--db` versus `SAKUR4_DB`, both
  backends (`none` and `embedded` behave identically), a missing `folds` table, the scalar queries
  themselves (all answer correctly by hand), read-after-write within one `Arc<Mutex<Connection>>`
  (the library tests rely on it and pass), the daemon's store path (`build_config` copies `cli.db`,
  resolved once in `main.rs`), a duplicate `--db` field shadowing the resolved one, the daemon's
  store access generally (`doctor`, `memory.recall` and `memory.staleness` all see the row), and
  request ordering (the serialised-dispatch experiment above).

  **Persistence is not in question**: the row is on disk, and a second session opens the same file
  and reports `episodes 1`. Only the session that wrote it, in the same pipelined batch, reports
  zero — which is the shape of a read served from state captured before the write rather than one
  served from a different store.

  **And a separate hazard this exposed:** `stats()` builds a `scalar` closure ending in
  `unwrap_or(0)`, so any query failure becomes a plausible-looking zero. Its comment says the intent
  was to avoid failing `doctor`; the effect is that `doctor` and `status` cannot distinguish "none"
  from "could not tell". That is worth fixing on its own.

  **This is also now breaking a check.** `verify.mjs`'s `context engine (FR-16)` fails locally with
  `2 contract(s) failed: something was evicted, compression_count advanced`, which is the same
  symptom the CI job showed for five rounds before I attributed it to a stale binary. Those
  contracts read daemon-reported state, so a daemon that reports zero for everything fails them —
  and the earlier conclusion that it was a build problem was wrong, or at best incomplete.

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
