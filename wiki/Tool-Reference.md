Every MCP tool Sakur4 exposes — its arguments, and **when an agent should reach for it**.

---

**On this page:** [At a glance](#at-a-glance) · [Calling convention](#calling-convention) · [Memory](#memory) · [Context](#context) · [Code intelligence](#code-intelligence) · [Session](#session) · [Operability](#operability) · [Resources](#resources) · [Prompt](#prompt)

Sakur4 advertises **17 tools**, **4 resources** and **1 prompt**, targeting MCP revision **2026-07-28** with `ttlMs` and `cacheScope` on list responses.

## At a glance

| Tool | Group | Required arguments |
|---|---|---|
| `memory.commit_episode` | Memory | `role`, `content` |
| `memory.pin` | Memory | `content` |
| `memory.recall` | Memory | `query` |
| `memory.fold` | Memory | `description`, `goal` |
| `memory.unfold` | Memory | `fold_id`, `result_summary` |
| `memory.recall_fold` | Memory | `fold_id` |
| `memory.staleness` | Memory | — |
| `context.receipt` | Context | — |
| `context.plan_eviction` | Context | `session_id` |
| `context.record_usage` | Context | `prompt_tokens` |
| `code.get_repo_map` | Code intelligence | `token_budget` |
| `code.query_symbol` | Code intelligence | `qualified_name` |
| `code.impact_of_change` | Code intelligence | `qualified_name` |
| `session.snapshot` | Session | — |
| `session.restore` | Session | `path` |
| `sakur4.status` | Operability | — |
| `sakur4.dream` | Operability | — |

## Calling convention

> **If you batch tool calls, await each answer before sending the next.**

JSON-RPC permits a server to process messages "as a set of concurrent tasks, processing them in any order", and MCP's stdio transport correlates responses only by `id`. Sakur4 relies on the order: the preamble tells the model to commit a turn and *then* consult what it remembers, and the tools are stateful in exactly that way. Sending each call and awaiting its answer is correct, and is what every harness tested here does.

Over streamable HTTP the request also carries the revision in a per-request `_meta` block, and the SEP-2243 headers `MCP-Protocol-Version` and `Mcp-Method` — plus `Mcp-Name` for `tools/call` — are required. The daemon requires that `_meta` block on `resources/read` exactly as on `tools/call`; omitting it returns `400 Invalid params: request _meta is missing`.

## Memory

Where turns become a record, and constraints survive compaction.

### `memory.commit_episode`

Append a turn or tool result to the Episodic Stream. **This is the source of truth: content is never rewritten.** Structured tool output (JSON, CSV, HTTP headers, diffs, exit codes) is additionally parsed into deterministic facts for the Symbolic Ledger.

| Argument | Required | Notes |
|---|---|---|
| `role` | **yes** | `system`, `user`, `assistant`, `tool` or `internal`. |
| `content` | **yes** | The turn or tool result, **verbatim**. |
| `tool_name` | no | Set for tool results, so the symbolic extractor can pick a parser. |
| `session_id` | no | Defaults to the session the daemon derived. |
| `slot_id` | no | Defaults to slot 0. |
| `corrects` | no | The `episode_id` this turn corrects. See below. |

**Call it when** every turn and every tool result lands — the working preamble says *after each turn or tool result*. Commit the user's words verbatim rather than paraphrasing: their words are the source everything else derives from, and a paraphrase loses exactly the part that matters.

Pass `tool_name` on a tool result. Without it a `git diff` is just text and nothing downstream can anchor to it. If the turn appears to state a constraint, a pin is **suggested** in `suggested_anchor` but never applied automatically.

#### Correcting an earlier turn

`episodic_stream` is **append-only** — a trigger blocks any change to a recorded row's content — so a correction is a *new* turn that references the one it corrects:

```jsonc
// "the port is 8080" was wrong; say so in a new turn that names it
memory.commit_episode {
  "role": "user",
  "content": "correction: the port is 9090",
  "corrects": "ep_01a0d50058a877a6b0ee89076ee"
}
```

The named episode is marked `superseded_by` this turn and flagged **droppable**, and a `Supersedes` edge is written between them. That matters beyond bookkeeping: the eviction engine scores a superseded episode **−3.0** with the note *"superseded by a later episode"*, so a turn that has been corrected becomes a **safe candidate to evict** instead of something the engine keeps for lack of a reason to let it go.

The response echoes `supersedes` back. **An unknown id is an error**, not a silent success — a caller who means to correct something and mistypes the id has not corrected it.

Before this existed, the column, the edge kind, the scoring rule and the note string were all present and **unreachable**: there was no way to say "this corrects that", so nothing ever set them.

### `memory.pin`

Pin a constraint, correction or task contract into the Anchor Set. Pinned content is **exempt from every eviction tier** and is rendered verbatim into every assembled prompt.

| Argument | Required | Notes |
|---|---|---|
| `content` | **yes** | The rule, in the user's own words. |
| `kind` | no | `safety_constraint`, `user_correction`, or `task_contract`. Defaults to `task_contract`. |
| `session_id` | no | Scope the pin to one session. |

**Call it when** the user states a rule, corrects you, or sets a hard requirement — immediately, not at the end of the task. **Unpinned requirements can be compacted away; pinned ones cannot.** The response reports the pin's `token_cost` and says so plainly: it now costs that many tokens in every prompt for the rest of the session.

Each pin costs tokens on **every** turn, so pin rules and corrections — not status updates. If the Anchor Set alone would exceed the context budget, pinning does not silently drop anything; the system reports the overflow instead.

### `memory.recall`

Search the Memory Fabric with lexical, dense and graph retrieval merged and reranked.

| Argument | Required | Notes |
|---|---|---|
| `query` | **yes** | Natural language is fine — retrieval unions terms beyond two rather than requiring all of them. |
| `k` | no | How many hits (clamped to 1–100). |
| `session_id` | no | Restrict to one session. |
| `file_path` | no | Restrict to material associated with a path. |
| `include_folded` | no | Include folded subtask traces. |
| `project_id` | no | Search another project **deliberately**. Omitting it is the normal case and is what keeps one project's transcript out of another's answers. |

**Call it when** you cannot remember something. **Do not guess — ask the store.**

Any interpretation whose source has since changed is returned with `stale: true` and the source's **current value** attached. Trust `current_value`, not `text`. Episodes are returned verbatim; eviction never alters stored content.

### `memory.fold`

Open an isolated sub-context: a checkpoint is taken at the current position, and everything until `memory.unfold` is attributed to the fold.

| Argument | Required | Notes |
|---|---|---|
| `description` | **yes** | Short description of the subtask. |
| `goal` | **yes** | What you are trying to find out. |
| `session_id` | no | Defaults to the derived session. |
| `slot_id` | no | Defaults to slot 0. |

**Call it when** you expect to read many files, run many greps, or explore an approach you may abandon — anything that will take more than a few steps of exploration. The work inside the fold costs almost nothing to have done, because the intermediate steps never occupy the main trajectory. The response returns a `fold_id` and a `checkpoint`; keep the id.

### `memory.unfold`

Close a fold: its intermediate steps leave the live window and only the result summary remains. The full trace stays retrievable via `memory.recall_fold`, and the inference slot is **rolled back** to the checkpoint taken when the fold opened.

| Argument | Required | Notes |
|---|---|---|
| `fold_id` | **yes** | The id `memory.fold` returned. |
| `result_summary` | **yes** | One line — what the subtask established. |
| `session_id` | no | Defaults to the derived session. |
| `slot_id` | no | Defaults to slot 0. |

**Call it when** the subtask is finished. The response reports `tokens_reclaimed`, `episodes_folded` and whether the rollback actually happened.

### `memory.recall_fold`

Retrieve every episode of a folded subtask, **verbatim, in order**.

| Argument | Required | Notes |
|---|---|---|
| `fold_id` | **yes** | The fold to expand. |

**Call it when** the collapsed result summary turns out to be insufficient — you need the steps, not the conclusion. This is the only way back to a fold's interior once it has been closed.

### `memory.staleness`

Report which stored interpretations no longer match the source they were derived from. Each entry names its anchor and whether the anchor changed or disappeared.

| Argument | Required | Notes |
|---|---|---|
| `limit` | no | Defaults to `50`, clamped to 1–500. |
| `project_id` | no | Scope the report to one project. |

**Call it when** deciding what to re-read rather than trusting a summary written a while ago — and at the end of a session, because the **next** session inherits whatever went stale in this one. The response reports `total`, `stale`, `stale_rate` and `deleted_anchors`, so you can see whether this is one entry or a habit.

## Context

The budget, the eviction decision, and what the provider charged.

### `context.receipt`

Show where the context budget went this turn, category by category, plus the **cache verdict**.

| Argument | Required | Notes |
|---|---|---|
| `session_id` | no | Defaults to the derived session. |
| `assemble` | no | Assemble the prompt now and observe it, instead of reporting the latest stored receipt. |

The breakdown accounts tokens to `system_prompt`, `pinned_anchors`, `retrieved_memory`, `repo_map`, `raw_recent_history`, `tool_schemas`, `fold_summaries` and `other`. The cache verdict is one of *reused the prefix*, *had to re-prefill*, or *warm-restored*.

**Call it when** a turn felt slow or cost more than expected, or to check whether compaction is actually paying for itself. `provider_cache` is present only when the harness has been reporting usage through `context.record_usage` — **absent means absent, not zero**.

### `context.plan_eviction`

Ask the Graduated Eviction Engine what it would evict right now, and why. Each update names the tier it moves to and the reason, and the response includes the cache verdict for the resulting boundary.

| Argument | Required | Notes |
|---|---|---|
| `session_id` | **yes** | The session to plan against. |
| `slot_id` | no | Defaults to slot 0. |
| `apply` | no | **Plan only by default.** Nothing changes until you pass `apply: true`. |
| `pending_recall` | no | Account for memory you are about to inject. |

The four tiers are `masked` → `referenced` → `archived` → `dropped`, and a tier is never skipped. Every plan ends in one of four reported verdicts: `aligned`, `snapped`, `partial-reuse` or `full-re-prefill`.

**Call it when** before a long or uncertain stretch of work — with `apply` **omitted** — to see what would be evicted and whether the resulting boundary reuses the inference cache. Pass `apply: true` only when you accept it. `applied` in the response tells you whether anything actually moved.

### `context.record_usage`

Report the token usage your provider returned for the last request, so Sakur4 can account for prompt-cache behaviour across the session.

| Argument | Required | Notes |
|---|---|---|
| `prompt_tokens` | **yes** | Prompt tokens for the request. |
| `completion_tokens` | no | Defaults to `0`. |
| `total_tokens` | no | Provider total, when it reports one. |
| `cache_read_tokens` | no | Prompt tokens served from the provider's prompt cache. |
| `cache_write_tokens` | no | Prompt tokens written into the cache this turn. |
| `reasoning_tokens` | no | Reasoning tokens, when the provider reports them. |
| `provider` | no | Provider name. |
| `model` | no | Model name. |
| `session_id` | no | Defaults to the derived session. |
| `slot_id` | no | Defaults to slot 0. |

**Call it when** your harness gives you usage from the model response — once per turn. Field names differ by provider; normalise to `cache_read_tokens` from whichever field you have:

| Provider | Field to read |
|---|---|
| OpenAI | `prompt_tokens_details.cached_tokens` |
| Anthropic | `cache_read_input_tokens` / `cache_creation_input_tokens` |
| DeepSeek | `prompt_cache_hit_tokens` |
| Gemini | `cachedContentTokenCount` |
| Groq, others | often absent — **omit the flag rather than sending zero** |

**Omitting is not the same as zero.** Zero asserts a cache miss; omitting says the provider did not report one, and Sakur4 says so rather than blaming a cache it cannot see.

The returned `verdict` is one of `not-reported`, `cache-miss`, `partial-reuse`, `full-reuse` or `PREFIX-BROKEN`, and `regression: true` means this turn was **billed for history that had already been paid for** — the signature of a compaction that rewrote already-sent history.

## Code intelligence

Parser output, not memory — this is the part that **cannot be stale**.

### `code.get_repo_map`

A structural outline of the repository, ranked by how load-bearing each symbol is in the call graph and fitted to a token budget.

| Argument | Required | Notes |
|---|---|---|
| `token_budget` | **yes** | How many tokens the map may occupy. |
| `focus_paths` | no | Boost symbols reachable from these paths. |
| `names_only` | no | Print **qualified names** instead of signatures. |

**Call it when** before opening files in full — it is cheaper than listing files. A smaller budget returns a strict prefix of what a larger budget returns, so it is safe to call repeatedly.

Set `names_only: true` to learn what to pass `code.query_symbol` and `code.impact_of_change`: the default map shows signatures, and a signature is not a qualified name. The map is built from the **last index**, and nothing re-indexes automatically — the body records when that index ran.

### `code.query_symbol`

Look up a symbol's **current** signature, location and AST hash from the Symbolic Ledger.

| Argument | Required | Notes |
|---|---|---|
| `qualified_name` | **yes** | The shape is `path::Type::member` — a method is qualified by the type it is implemented on. |

**Call it when** you are unsure whether something you remember is still true. This is parser output, not memory: **it cannot be stale**, and it is the right way to check something you only remember from a summary. A name that does not exist comes back with `found: false` and a note pointing at `code.get_repo_map` to see the names in use.

### `code.impact_of_change`

List every call site that depends on a symbol, **transitively**, from the pre-computed call and import graph.

| Argument | Required | Notes |
|---|---|---|
| `qualified_name` | **yes** | As above. |
| `depth` | no | Defaults to `4`, clamped to 1–16. |

**Call it when** before a signature change, so the edit does not break callers you have not read. Each caller carries `depth`, how it reaches the symbol (`via`), and a structured `stale` flag meaning its code has changed since it last saw this symbol.

## Session

Persisting and reloading the inference server's view of the session.

### `session.snapshot`

Persist the slot's KV cache to disk **immediately**.

| Argument | Required | Notes |
|---|---|---|
| `session_id` | no | Defaults to the derived session. |
| `slot_id` | no | Defaults to slot 0. |

**Call it when** before a long generation, before an unavoidable rewrite, or at the end of the day. It turns a 60–120 second cold prefill later into a **sub-second restore**. Save files go to the directory named by `SAKUR4_SNAPSHOT_DIR`, defaulting to the temporary directory. A backend that cannot save says so rather than reporting an internal failure.

### `session.restore`

Reload a slot's KV state from a save file produced by `session.snapshot`.

| Argument | Required | Notes |
|---|---|---|
| `path` | **yes** | The save file returned by `session.snapshot`. |
| `session_id` | no | Defaults to the derived session. |
| `slot_id` | no | Defaults to slot 0. |

**Call it when** before the first turn of a new day, to avoid paying for a full re-prefill of an existing session.

> **Snapshots are as sensitive as the store.** A slot-save file is 60–500 MB of model state representing everything the session has seen, and nothing encrypts them.

## Operability

### `sakur4.status`

Report the resolved backend, its detected cache capabilities, the tokenizer and embedder in use, and counts for each Fabric store.

**Required arguments:** none.

**Call it when** once at the start of a session. `cache_coherence` tells you whether cache-coherent compaction is available or whether you should pin more aggressively. The `episodes`, `symbolic_facts`, `atlas_entries` and `anchors` fields are **store-wide** — one store holds every project a user has worked on. Read `project_episodes`, `project_facts` and `project_atlas` when asking what *this* project remembers, and `store_holds_other_projects` when a count looks low. `open_folds` tells you whether a fold is still outstanding: a fold opened and not closed is a piece of your own working state.

### `sakur4.dream`

Run one Idle Consolidator pass: promote substantial turns into the Semantic Atlas, regenerate summaries whose source changed, embed anything missing a vector, and archive long-cold episodes.

| Argument | Required | Notes |
|---|---|---|
| `force` | no | Ignore the quiet-period gate. Only sensible when you know the slots are idle; the consolidator still refuses to overlap a generation it can observe. |

**Call it when** you are between tasks and nothing is generating. It **refuses to run while any tracked slot is generating**, and is safe to call at any time. The response reports `ran`, `promoted`, `regenerated`, `reembedded` and `archived`, or `skipped_reason` when it declined.

## Resources

Four URIs, all returning `text/plain`. `{project}` is the project the daemon was started for.

| URI | Contents | Caching |
|---|---|---|
| `sakur4://repo-map/{project}` | The structural outline of the repository from the last index. | Cached briefly; the **body dates itself**, and nothing re-indexes automatically. |
| `sakur4://receipt/latest` | The most recent Context Ledger Receipt. | **Never cached** — it describes one turn. |
| `sakur4://anchors/{project}` | Every pinned constraint. | Invalidated by any `memory.pin` call. |
| `sakur4://status/{project}` | Backend, capabilities and store counts. | — |

The Anchor Set resource is worth reading **every turn**: a pinned constraint reaches the model only if something puts it there, and a short rule like "never force-push to main" will never resemble the user's message enough to come back from `memory.recall`.

## Prompt

**`sakur4_system_preamble`** — the reusable system-prompt fragment. It names **when** to call each tool rather than what each tool does.

The failure mode with smaller instruction-tuned models is **under-triggering**: they have the tools and do not reach for them. Numbered triggers fixed that in testing; a prose description did not. That is why the prompt reads as a list of conditions — commit after each turn, fold before a long exploration, pin immediately on a rule, check rather than trust, and read the cache line when a turn is slow.

Harness adapters may inject it automatically. The OMP extension injects it **once per session** on `before_agent_start`.

---

<sub>[← Back to Home](Home) · [All pages](Home#where-to-go-next)</sub>
