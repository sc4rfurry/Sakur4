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

## How it closes the gap

Three commitments, and they are what the architecture is shaped around.

**Memory is two tracks.** Deterministic parser facts live in the Symbolic Ledger,
model interpretation lives in the Semantic Atlas, and there is no path from a model
into the first. A summary that has drifted from its source is caught at read time by
comparing hashes, not trusted because it was written.

**Eviction is deterministic.** The Graduated Eviction Engine selects what to compress
from token counts, recency, graph in-degree and explicit droppability — no model in
the decision path, so a plan is reproducible and cannot hallucinate.

**Compaction is cache-aware.** The Cache-Coherence Layer decides *where the eviction
boundary may fall* by asking what the inference server can actually rewind to, so the
surviving prompt head stays a prefix of what the server holds.

Jump to [Status](#status) for what is verified versus not,
[What is implemented](#what-is-implemented) for the component map, or
[the novel part](#the-novel-part-and-how-to-see-it-working) for the cache-coherence
claim and how to watch it work.

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
```

---

## Using Sakur4 from a harness

Sakur4 is an MCP server. Any harness that speaks MCP can use it; nothing in the
tool surface depends on which harness is calling.

```bash
# Print ready-to-paste configuration for your harness.
sakur4d config hermes          # ~/.hermes/config.yaml
sakur4d config claude          # claude_desktop_config.json
sakur4d config claude-code     # one-line CLI registration
sakur4d config generic-http    # for anything that connects to a URL
sakur4d config generic-stdio   # for anything that spawns a child process
```

### Two transports, because harnesses disagree

| Transport | How the harness reaches it | Use it when |
|---|---|---|
| **stdio** (default) | spawns `sakur4d` as a child and speaks JSON-RPC over its pipes | one harness, simplest setup, no port to manage |
| **streamable HTTP** | connects to a URL | several sessions sharing one store, or a harness on another machine |

```bash
sakur4d serve --transport stdio                        # default
sakur4d serve --transport http --bind 127.0.0.1:8765    # shared
```

### What Sakur4 exposes

**17 tools** — `memory.commit_episode`, `memory.pin`, `memory.recall`,
`memory.fold`, `memory.unfold`, `memory.recall_fold`, `memory.staleness`,
`code.get_repo_map`, `code.query_symbol`, `code.impact_of_change`,
`session.snapshot`, `session.restore`, `context.receipt`,
`context.plan_eviction`, `context.record_usage`, `sakur4.status`, `sakur4.dream`.

**4 resources** — `sakur4://repo-map/{project}`, `sakur4://receipt/latest`,
`sakur4://anchors/{project}`, `sakur4://status/{project}`.

**1 prompt** — `sakur4_system_preamble`, a preamble tuned for smaller models that
under-trigger folding, naming *when* to call each tool rather than what it does.

### Verified against Hermes Agent

```
$ hermes mcp test sakur4
  Testing 'sakur4'...
  Transport: stdio → D:\DuDu\Sakur4\target\debug\sakur4d.exe
  ✓ Connected (5765ms)
  ✓ Tools discovered: 17
```

### OMP (Oh My Pi) — native extension

OMP 18.1.17 has **no MCP client**, so it cannot be reached the MCP way. It has a
native TypeScript extension API instead, and `integrations/omp-plugin/` implements
it: nine tools, a `/sakur4` command, and hooks that a tool provider cannot reach —
automatic provider-usage reporting, memory injected before each turn, and
compaction that defers to Sakur4's eviction plan.

```bash
cargo install sakur4d
node integrations/omp-plugin/install.mjs
```

Restart OMP; it should report nine `sakur4_` tools. See
[integrations/omp-plugin/README.md](integrations/omp-plugin/README.md).

### Skills — for any harness that reads the Agent Skills standard

`skills/sakur4/` is a portable [Agent Skills](https://agentskills.io/specification)
package: a `SKILL.md` plus a dependency-free Node CLI that drives the daemon. It
works in OMP, Claude Code, Codex, pi, and anything else that reads
`~/.agents/skills/`.

```bash
node integrations/omp-plugin/install.mjs --skill-only   # into ~/.agents/skills/
```

The skill is progressive disclosure: only its description sits in context until a
task matches, at which point the model loads the full instructions. That means a
harness with no MCP support and no extension system still gets Sakur4 — the model
runs `node scripts/sakur4.mjs commit …` directly.

Everything the skill teaches is also reachable over MCP, so a harness can use
either path, or both.

---

## Cache accounting, local and cloud

The Cache-Coherence Layer talks to a local `llama.cpp` slot: it asks the server
where its KV cache can be rewound to, and aligns the eviction boundary to it.

A hosted provider has no such API — but it reports, in every response, how many
prompt tokens came from its prompt cache. Hermes' own documentation calls
per-turn compaction's cache invalidation "the strongest argument against it", and
notes the trade depends on numbers specific to you. Sakur4 can supply them:

```jsonc
// The harness reports what the provider said.
context.record_usage { "session_id": "s", "prompt_tokens": 6200,
                       "cache_read_tokens": 300 }

// Sakur4 answers with a verdict.
{ "verdict": "PREFIX-BROKEN", "regression": true,
  "detail": "the provider's cached prefix fell from 5000 to 300 tokens (4700 tokens
             no longer cached) while the prompt went 6000 → 6200; this turn was
             billed for history that had already been paid for" }
```

The signature is an inversion: append-only growth makes the cached prefix *grow*,
while a rewrite that replaces a long prefix with a shorter one makes it *shrink*
even as the prompt stays large. That inversion is detectable, and it is what
`context.receipt` now reports per session.

Sakur4 never calls a provider itself — it is a subsystem, not a harness. The
harness pushes the numbers; Sakur4 does the accounting and the eviction.

---

## Status

v0.1.0. 223 tests pass, CI runs on Linux, macOS and Windows with warnings denied,
and the tool surface is verified against a real Hermes Agent install.

"Production ready" is a claim that should come with evidence, so here is what that
means precisely.

**Verified**

- The MCP surface works over both transports. The stdio tests spawn the real binary
  and speak JSON-RPC over its pipes; `hermes mcp test sakur4` connects to a real
  Hermes install and discovers all 17 tools.
- The cache-coherence claim is stated as executable contracts in
  `crates/sakur4-core/tests/cache_coherence.rs`, and `sakur4d demo` shows it working.
- Durability: killing the process mid-session preserves the store, and the session
  resumes with every committed turn intact.
- Malformed input is rejected per call without killing the server, including a
  large-payload round trip.
- Library code contains zero `unwrap`/`expect`/`panic!` paths outside tests.
- `cargo publish -p sakur4-core --dry-run` builds the extracted archive in
  isolation, so the published crate is known to compile standalone.

**Not verified**

- **No real `llama.cpp` server has been contacted.** The adapter is tested against a
  fake server that speaks the documented routes over real HTTP, so the *client* is
  exercised, but first contact with a real build is still first contact.
- **No live agent session has driven these tools under a real model**, so the tool
  descriptions have not been tested against a model's judgement about when to call
  them.
- **OMP cannot use Sakur4 over MCP**, because OMP has no MCP client.
- **Optional encryption at rest (FR-20) is not implemented.**
- **`cargo deny` / `cargo audit` are not wired into CI.**

## What is implemented

| Component | Status | Where |
|---|---|---|
| **C1 Memory Fabric** — Episodic Stream, Symbolic Ledger, Semantic Atlas, Anchor Set, Dependency Graph | done | `crates/sakur4-core/src/memory/` |
| **C2 Graduated Eviction Engine** — four tiers, dependency-graph walk, `fold`/`unfold` | done | `crates/sakur4-core/src/evict.rs` |
| **C3 Cache-Coherence Layer** — capability probing, checkpoint-aligned boundaries, snapshot/restore, graceful fallback | done | `crates/sakur4-core/src/cache/`, `crates/sakur4-core/src/llama/` |
| **C4 Repo Cortex** — tree-sitter AST/call/import graph, incremental re-index, token-budgeted repo map, impact query | done | `crates/sakur4-core/src/repo.rs` |
| **C5 Hybrid Recall** — BM25 + dense + graph, staleness-aware rerank | done | `crates/sakur4-core/src/recall.rs` |
| **C6 Idle Consolidator** — promotion, staleness regeneration, re-embedding, cold archival | done | `crates/sakur4-core/src/consolidate.rs` |
| **C7 MCP Gateway** — 17 tools, 4 resources, 1 prompt, stdio + HTTP, spec 2026-07-28 | done | `crates/sakur4d/src/tools.rs`, `crates/sakur4d/src/gateway.rs` |
| **C8 Context Ledger Receipt** — per-turn token, cache, and provider-cache accounting | done | `crates/sakur4-core/src/receipt.rs`, `crates/sakur4-core/src/provider_cache.rs` |
| **C9 Harness Adapters** — Hermes MCP registration, OMP native extension, portable Agent Skill | done | `integrations/omp-plugin/`, `skills/sakur4/` |

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

---

## Against a real llama.cpp server

```bash
llama-server -m model.gguf -c 65536 --slots -cms 256 -ctxcp 64

sakur4d --backend http://127.0.0.1:8080 doctor
sakur4d --backend http://127.0.0.1:8080 serve
```

`--backend` accepts `auto` (probe, fall back to embedded), `embedded`, `none`, or any
base URL — including one on another machine. `doctor` prints exactly which cache
endpoints were detected and what that means for compaction.

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
  provider_cache.rs     C8 — prompt-cache accounting for hosted providers
  prompt.rs             the single prompt assembler every budget decision uses
  tokens.rs             the single tokenizer every budget decision uses
  store/                SQLite schema, migrations, lexical and vector search
crates/sakur4d/         the daemon: CLI + MCP gateway (stdio and HTTP)
crates/sakur4-testkit/  fixture repos, a fake llama.cpp server, a scripted session driver
integrations/omp-plugin/  native Oh My Pi extension + installer
skills/sakur4/          portable Agent Skills package + a dependency-free Node CLI
docs/DESIGN.md          how each requirement is met, and the trade-offs taken
docs/RELEASING.md       how to cut a release, and what is manual and why
```

The behavioural contracts live in `tests/` at the crate roots:

| File | What it pins down |
|---|---|
| `sakur4-core/tests/cache_coherence.rs` | the central claim: a compaction preserves a prefix the cache can reuse |
| `sakur4-core/tests/repo_parsing.rs` | what ends up in the Symbolic Ledger for each language |
| `sakur4d/tests/gateway.rs` | the tool surface over real HTTP, driven by the SDK's own client |
| `sakur4d/tests/stdio_transport.rs` | the tool surface over stdio, driving a spawned `sakur4d` |

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

* **Hermes' deeper integration points are unused.** Hermes exposes a `ContextEngine`
  base class whose `update_from_response` already carries `cache_read_tokens` and
  `cache_write_tokens`, and whose `compress()` hook could delegate eviction to
  Sakur4 in-process. Today Hermes reaches Sakur4 as a tool provider only — which
  works, but means cache accounting has to be reported explicitly rather than
  arriving automatically on every turn. The OMP extension does use its equivalent
  hooks; Hermes could do the same.
* **The OMP extension's compaction hook has not been exercised against a real
  compaction.** The tool path is verified end to end with a live model, but forcing
  OMP past its context limit to observe `session_before_compact` is a separate piece
  of work.
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
* **Hermes has not been driven by a live model.** Hermes' *transport* is verified —
  `hermes mcp test sakur4` connects and discovers all 17 tools — and the OMP tools
  have been driven end to end by a live model, but no agent conversation has run
  through Hermes itself, so its tool descriptions have not been exercised against a
  model's judgement about when to call them.
* **`acceptance_criteria` covering external systems** — an MCP conformance run
  against the reference client, LoCoMo, the Endurance Benchmark — have not been run.
