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

## [0.2.1] - 2026-09-25

**A patch, per this file's own policy:** nothing was added to, removed from or renamed in the MCP tool
surface. The only change to it is a description string (`code.get_repo_map`'s, which promised a
strict-prefix behaviour no truncation marker can deliver).

**Upgrading from 0.2.0 is worth it for one thing above all: the pipelined-read ordering defect is
fixed.** It was the headline limitation of this project, it could silently hand a session a stale count,
and it is gone.

### Fixed

* **A batched read is no longer served before a write it was sent after.** JSON-RPC permits a server to
  process a batch *"in any order"*, and MCP correlates responses only by `id` — so a `memory.commit_episode`
  and a `sakur4.status` written together could be answered in either order, and the status could report the
  count from before the commit. **The stdio transport now withholds message N+1 until the response to N has
  been written.**

  Measured, twelve commit-then-status pairs per run, every frame written before any reply is read:

  ```text
  0.2.0:  replies 25/25   statuses 12/12   WRONG 5, 1, 5, 2, 4, 3
  0.2.1:  replies 25/25   statuses 12/12   WRONG 0, 0, 0, 0, 0, 0
  ```

  **Fourteen attempts; six of them tried to order the handlers with a lock, which cannot work** — a lock
  orders handlers, not dispatch. The record is in `docs/DESIGN.md`, and the last attempt succeeded only
  after instrumenting a counter instead of reasoning about it. Two mistakes made on the way are now each
  pinned by a test: a notification produces no reply and must not be waited on, and end of input is not the
  end of output — a transport that closes its read side at EOF truncates a large reply such as `tools/list`.

* **A latency figure taken on a busy machine is no longer reported as a regression.** `NFR-2 recall at
  scale` reported 1352.9 ms in a full run and 23.0 ms alone, because the benchmark was measuring 261
  concurrent tests. The verdict is now `PASS`, `FAIL` or `INCONCLUSIVE`, and the last of those is recorded
  as a skip carrying the load rather than as a failure.

* **`code.get_repo_map`'s description no longer promises a strict prefix.** It said a smaller budget returns
  a strict prefix of a larger one and that was false at three budget pairs out of three; the footer's
  variable-width counts moved the divergence. The counts are gone (they are in the structured result) and the
  description states what is measured.

* **`FtsStore::search_episodes` no longer claims to exclude unattributed rows.** Its doc comment said a
  scoped search excludes `project_id IS NULL` "rather than treating NULL as a match"; the predicate is
  `(?p IS NULL OR project_id = ?p)`, so `None` is a wildcard over **every** project. The tool path was never
  affected — `memory.recall` always binds a project — but a guarantee was stated that no test checked.

### Added

* **`install.ps1` runs correctly on non-Windows hosts** by skipping rather than failing, with the platform
  decision asserted in both directions by its own test.
* Regression tests for the batch ordering, the notification reply count, and the batched tool catalog.

## [0.2.0] - 2026-09-24

**A minor bump rather than a patch**, per this file's own policy: the MCP tool surface gained an
argument (`memory.commit_episode`'s `corrects`), and the store gained a column and two migrations
(`episodic_stream.project_id`, `dependency_graph_edge.target_hash`). Adding to the surface is a minor
change; nothing was removed or renamed.

**If you are on 0.1.0 and it works for you, the reason to move is that 0.2.0 fixes four defects that
made advertised behaviour unreachable**, each described in `docs/DESIGN.md`:

* **A subtraction in a log message was panicking the daemon.** `Escalation::Proposed` formatted
  `tokens_before - tokens_after`, which underflows when a `Masked` stub is longer than the short episode
  it replaces — the case the ladder deliberately allows. The panic landed on an async worker, so
  `context.plan_eviction` never answered; the Hermes engine read the 20-second timeout as "Sakur4
  unreachable" and **left the context uncompacted on every long session**. `context engine (FR-16)` had
  been failing on this for many releases.
* **All four prompt-building paths bypassed FR-4's anchor budget check.** They assembled the anchor block
  with a `join`, so `render_anchor_block` — the only place that refuses when pinned anchors cannot fit,
  and the only place that orders them — had no production caller.
* **Projects were not isolated.** `episodic_stream` was the one table with no `project_id`; a session in
  one project could be handed another's transcript.
* **A backend limitation reached the wire as an internal failure.** NFR-7's degrade path existed as
  `Error::is_backend_unavailable` and was never called, so a server without `?action=save` reported
  `internal_error` — which reads as "Sakur4 is broken" rather than "this backend cannot do that".

**One thing 0.2.0 does not fix** is documented in [Limitations](https://github.com/sc4rfurry/Sakur4/wiki/Limitations)
and in `docs/DESIGN.md`: a **pipelined batch is dispatched in an arbitrary order**, so a read can be
served before a write it was sent after. Await each answer. Eight fixes were attempted and reverted.

### Added

**Corrections can be recorded (`memory.commit_episode`'s `corrects`)**

- Naming an episode in `corrects` marks it `superseded_by` the new turn and flags it **droppable**, which
  makes it a safe eviction candidate rather than something the engine keeps for lack of a reason to let
  it go. `MemoryFabric::mark_superseded` had no caller before this, so the eviction engine's −3.0 rule
  for superseded episodes could never fire.
- An id that matches nothing is a `NotFound` error, not a silent success.

**FR-11's caller staleness annotation (schema v4)**

- dependency_graph_edge records the hash the caller held when a call site was first observed, and
  impact_of_change compares it with the caller's hash today, so each affected site can be marked
  [STALE: changed since it last saw this symbol]. The field existed and was a constant alse
  because there was nothing to compare against.
- The annotation is now rendered. It was computed and shown nowhere — not by sakur4d impact and not
  by code.impact_of_change — which is a separate defect from the field being constant: a report
  that carries a verdict and does not print it is worse than one without the field, because the shape
  implies the annotation exists.
- Edges written before migration 4 have no recorded hash and report not-stale rather than
  not-checked; the value is left NULL rather than backfilled, because backfilling would assert that
  every existing call site is current, which is the claim there is no evidence for.
- The column is deliberately not refreshed on conflict: re-indexing the target says nothing about
  whether the caller re-read it, and refreshing would erase the drift the column detects.

**Episodes record their project (schema v3)**

- `episodic_stream` gained a `project_id` column and an index on `(project_id, seq)`. It was the one
  table without one — `semantic_atlas`, `symbolic_fact`, `repo_file` and `project` were all keyed by
  project — and it is the table the tools write on every turn, so a store holding several projects
  could not keep their transcripts apart. Reported from use: working in OMP in one project surfaced
  material from another.
- `sakur4.status` reports `project_episodes`, `project_facts` and `project_atlas` scoped to the
  project the daemon was started for, alongside the store-wide totals it already had — and
  `store_holds_other_projects`, so a count that looks low is explained rather than surprising. The
  store-wide fields remain for `doctor`, which is asking about the store.
- Existing rows keep `NULL` rather than being backfilled with a guess: an old episode's project is
  not recoverable, and attributing it to whichever project happened to be open at migration time
  would be worse than leaving it unattributed. A project-scoped query excludes those rows.
- **Still open:** episodic recall filters on `session_id` alone, so it does not yet use the new
  column. Semantic recall is project-scoped; the transcript is not.

**Episodic recall is project-scoped**

- `memory.recall` filters the transcript by the project the daemon was started for. It previously
  filtered on `session_id` only, so a caller that passed no session — which is the common case — got
  turns from every project in the store, and `session_id` is derived from the working directory's
  name by the OMP plugin, so two checkouts of `src` shared one.
- `memory.recall` accepts an optional `project_id` to search another project deliberately. This is
  additive: omitting it is the normal case, so no existing call changes meaning.
- **Breaking for anyone relying on the old behaviour.** A recall that used to return other projects'
  turns no longer does. That was the defect; a caller that wants it can pass a `project_id`.

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

- **The repo map dates itself, and its resource stops claiming a TTL it does not have.** The map is
  built from the last index and nothing re-indexes automatically — `notify` and
  `notify-debouncer-full` were declared for filesystem watching and referenced by no code path — so
  after an edit it describes the tree as it was, and `code.impact_of_change` reasons over the same
  stored facts. A map with no date reads as current, which is the condition under which a structural
  answer is most confidently wrong. The body now carries the index time, or says it has never been
  indexed, and names `sakur4d index`.
- The repo-map resource description said its TTL "tracks Repo Cortex's last re-index". It does not:
  the TTL is a constant, and a client would reasonably infer that a cached map is refreshed whenever
  the index moves. `RepoCortex::last_indexed` existed for exactly that and had no caller.
- No gateway test read a resource *body* — every existing one lists resources — which is why both
  survived. `the_repo_map_dates_itself` closes the gap.
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

- 265 tests, including integration tests
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
- **`cargo deny` is not wired into CI.** (`cargo audit` is, as of 0.2.0.) Review `Cargo.lock`
  changes in a pull request.

[Unreleased]: https://github.com/sc4rfurry/Sakur4/compare/v0.2.1...HEAD
[0.2.1]: https://github.com/sc4rfurry/Sakur4/releases/tag/v0.2.1
[0.2.0]: https://github.com/sc4rfurry/Sakur4/releases/tag/v0.2.0
[0.1.0]: https://github.com/sc4rfurry/Sakur4/releases/tag/v0.1.0
