# Cache Coherence

> The project's central bet: compaction can only save prefill work if the head of the new prompt is
> byte-identical to the head of the old one — so Sakur4 asks the inference server *where a cut can
> fall* before it decides **what** to evict.

---

**On this page:** [The problem, restated precisely](#the-problem-restated-precisely) ·
[What a checkpoint boundary is](#what-a-checkpoint-boundary-is) ·
[Choosing the boundary](#choosing-the-boundary) · [How the backend is probed](#how-the-backend-is-probed) ·
[The three eviction profiles](#the-three-eviction-profiles) ·
[What retained_prefix_tokens means](#what-retained_prefix_tokens-means) ·
[On a backend with no checkpoint API](#on-a-backend-with-no-checkpoint-api) ·
[Three corrections worth recording](#three-corrections-worth-recording) ·
[Partial-state architectures](#partial-state-architectures) ·
[The hosted-provider half](#the-hosted-provider-half) ·
[What is not established](#what-is-not-established)

---

## The problem, restated precisely

llama.cpp keeps a KV cache **per slot** and matches a new prompt against it by **longest common
prefix**. A harness that compacts by summarising produces a prompt whose *first* token differs from the
previous one, so the match length is zero and the entire compacted context is re-prefilled. The
operation whose whole purpose was to make the session cheap becomes the most expensive thing in it —
**100+ seconds on a 50K-token session on consumer hardware**, and the cost lands on the turn right after
the one that was already slow.

Nothing in that loop is *wrong*. A harness compacts because the window is filling, and llama.cpp
matches prefixes because that is the only thing a KV cache can do cheaply. The two systems simply do not
know about each other.

**The insight Sakur4 is built on:** a compaction can only save prefill work if the head of the new prompt
is byte-identical to the head of the old one. So the question is not *"which episodes are least
valuable"* — it is **"where can the head be cut such that the server already holds everything before
it"**. Sakur4 knows about both systems, so it asks the second question first.

This was measured on real hardware rather than argued. Against a locally served 27B model at Q3_K_XL,
3,045-token prompt, `n_predict = 1`, `cache_prompt = true`:

| Case | prompt processed | cache reused | wall clock |
|---|---|---|---|
| A — cold | 3,045 | 0 | **2,743 ms** |
| B — identical prompt again | 4 | 3,041 | **162 ms** |
| C — same prompt, head changed | 3,060 | 0 | **2,426 ms** |

- Prefill costs **0.89 ms/token** — about **1,118 tokens/second** on a 27B at Q3.
- Re-sending an identical prompt is **94% faster** — the server reports only 4 tokens processed.
- **Changing the head costs 15–17×** what the cached version costs, and it is the failure Sakur4 exists
  to prevent.

---

## What a checkpoint boundary is

A **checkpoint** is a token position the server can rewind its slot state to. llama.cpp builds a ring
of them (the `-cms` / `--ctx-checkpoints` machinery), and each entry is a position, not a copy of the
prompt. A boundaries decision is therefore *positional*: if the surviving prompt head reaches exactly a
checkpoint the server still holds, everything up to that point is reusable and only the suffix is
prefilled.

Sakur4's Cache-Coherence Layer knows about **four kinds** of checkpoint — the `cache_checkpoint.kind`
column:

| Kind | Where it comes from |
|---|---|
| `internal_checkpoint` | the backend's own in-memory ring |
| `slot_save_file` | a slot persisted to disk with `POST /slots/{id}/save` |
| `fold_marker` | the save point `memory.fold` takes when a fold opens |
| `pre_rewrite` | the snapshot taken before a rewrite that could not be aligned |

**The candidate list is a merge, not a query.** `plan_boundary` takes the backend's live ring, adds any
checkpoints **Sakur4 itself** recorded (fold markers, previous pre-rewrite saves), and drops duplicates
— a save file is usable even when the in-memory ring has wrapped past it. It then takes the slot's real
position as `max(n_past, bookkept)`, because the server is the authority on what it holds and Sakur4's
own bookkeeping can only ever be behind it.

The ring's arithmetic matters too: it only spans `interval × depth` tokens, so its **oldest entry is
usually well past a freshly-proposed boundary**. That fact is the source of correction #2 below.

---

## Choosing the boundary

`EvictionEngine::boundary_and_prefix` runs **before any eviction is chosen**:

1. Propose a boundary `cache_prefix_reserve_tokens` into the session.
2. Ask the coherence layer for the checkpoints it can actually rewind to — the backend's live ring
   merged with Sakur4's own recorded save points, filtered by what the model architecture permits.
3. Snap the proposal onto the **nearest checkpoint at or below it**, within tolerance.
4. If nothing usable lies below the proposal, align **forward** onto the oldest checkpoint the ring
   still holds, provided it is early enough to leave a middle worth evicting.
5. Preserve every episode before that boundary **verbatim**; absorb the pressure from what follows.

The decision order inside `plan_boundary` is explicit, and the order is the design:

```text
  1. backend unreachable ................................ full rewrite, reason recorded
  2. slot state unreadable .............................. full rewrite, reason recorded
  3. no checkpoints at all .............................. full rewrite: "no checkpoints are
                                                          available on this backend/session"
  4. LIVE HIT — n_past <= requested_cut ................. aligned at n_past
       (falls out of the server's own position; needs no itemised ring)
  5. EXACT HIT — a checkpoint sits at the cut .......... aligned, Δ0
  6. NEAREST BELOW — within snap tolerance (512) ....... aligned, snapped back
  7. FORWARD ALIGN — ring starts past the cut .......... keep more context; reuse from there
  8. otherwise ......................................... full rewrite, with the arithmetic:
                                                          how far back the nearest was, or that
                                                          the ring had already moved past
```

An earlier version checked *"is any checkpoint newer than the cut?"* first and reported a rewrite
whenever one was — which is true of almost every real request, and therefore **quietly disabled
alignment entirely**. That is why the order above is stated rather than inferred.

Two knobs bound the proposal so the prefix cannot eat the session:

| Knob | Default | Why |
|---|---|---|
| `cache_prefix_reserve_tokens` | 4,096 | how far into the session the proposal starts |
| `cache_prefix_max_tokens` | 8,192 | absolute ceiling, so a pathological ring cannot take the whole window |
| `max_prefix_ratio` | 0.35 | no more than a third of the live window, or there is no middle left to evict |
| `snap_tolerance_tokens` | 512 | how far a cut may move backwards to reach a checkpoint |

Without a prefix **floor** and a **ceiling**, "cache-aligned eviction" is not implementable: the
boundary has to be chosen with both the cache and the eviction budget in view.

### The verdict, and its evidence

Every plan carries a status, the boundary, and the **reason** — the reason is what the receipt prints,
so a slow turn is explainable rather than mysterious. The four documented verdicts are `aligned`,
`snapped`, `partial-reuse` and `full-re-prefill`. Note precisely what the wire carries, because the two
do not match:

- `aligned` and `snapped` are **descriptive**, both produced by `BoundaryPlan::aligned(…)`, which
  returns `partial-reuse`; the difference (Δ0 versus moved back) lives in the reason and the structured
  `snap` field — e.g. `snapped 128 tokens back onto an in-memory checkpoint`.
- The `cache_status` values a client actually sees are **`unknown`, `cold`, `full-re-prefill`,
  `partial-reuse`, `warm-restored`**.
- The layer also tracks a fourth verdict it does **not** emit, described below in
  [On a backend with no checkpoint API](#on-a-backend-with-no-checkpoint-api).

`cache_status` is not merely cosmetic: it is the numerator and denominator of the project's headline
metric ("percentage of compaction events resolved via partial-prefix reuse vs. full re-prefill"), which
is why the layer is careful about what it is willing to call reuse.

**`pairs_with_cache` is the honesty gate.** A preserved prefix only counts as reuse where the server can
actually match it. When no boundary was alignable — no backend, no checkpoints, or partial-state-only
checkpoints — the prefix is **still preserved** and still a prefix of the next prompt, but the flag is
false and the plan does not claim `partial-reuse`. Reporting it there *"would put a number on G1's
headline metric that nothing supports"*.

**The fallback is a first-class path (NFR-7).** `BoundaryPlan::full_rewrite` is returned — **with its
reason** — for an unreachable backend, a build with no `/slots`, a build whose ring is unusable, and
sliding-window or hybrid models whose checkpoints carry only partial state. The plan still **evicts**;
it reports `full-re-prefill` and says why. The original risk register rates this integration as the
project's highest risk, so the failure mode was made "slower, and honest about it" from the start.

---

## How the backend is probed

`resolve()` chooses between the three implementations — the llama.cpp HTTP adapter, an embedded
simulation, and a null backend for "cache coherence is off" — from `auto` / `embedded` / `none` / a URL.
`auto` probes and falls back to embedded, so a developer with no server running still gets a fully
functional Sakur4 whose cache behaviour is **observably simulated**: the backend's name appears in the
receipt.

`LlamaCppBackend::connect` performs an **ordered probe**, and every step is independent and tolerant —
a 404 marks one capability false rather than failing the connection. That is what lets the same binary
drive a current build with a full checkpoint ring and a two-year-old build without a configuration flag.

| Step | Route | What it establishes |
|---|---|---|
| 1 | `GET /health`, `GET /props` | is anything there, and what model is it |
| 2 | `GET /slots` | slot state, `n_ctx`, and on builds that expose it the checkpoint ring |
| 3 | `GET /props` | the model **architecture string**, from which partial-state-only is inferred |
| 4 | `POST /tokenize` with a known canary | exact token accounting is available |
| 5 | `GET /metrics` | prompt-eval telemetry |

The checkpoint ring is read from the first slot's payload under **any** of the field names seen in the
wild — `checkpoints`, `ctx_checkpoints`, `context_checkpoints`, `ckpt` — because the spelling has moved
between revisions.

**On save and restore specifically, the code is conservative rather than optimistic.** A ring implies
the server-side save/restore verbs exist, so `slot_save` and `slot_restore` are set from
`checkpoint_ring`; otherwise they are treated as **absent** until a probe confirms the route, "because on
some builds the route exists but the handler needs a compiled-in flag". `slot_erase` is set from
`slots`.

If you are looking for the route in the code or in the recorded probe output, be aware that **two
spellings appear in this repository and they are not the same thing**:

- The adapter's own save verb is **`POST /slots/{slot_id}/save`** (with `POST /slots/{slot_id}/restore`
  and `POST /slots/{slot_id}/erase` beside it).
- The recorded capability probe against a real server tested **`/slots/0?action=save`** and
  `/slots/0?action=erase`, and both returned **501**. That is a *different route shape*, and on that
  build neither existed.

`doctor` reports the whole picture in three lines, and the middle one is the one that matters for
compaction:

```console
$ sakur4d --backend http://your-llama-server:8080 doctor
  capabilities     slots+tokenize
  coherence        no checkpoint source detected — compaction will report full re-pre-fill
  eviction         window-first · trigger 70% · target 30% · keep 4096 recent
```

The capability set is more granular than those three lines: `slots`, `slot_save`, `slot_restore`,
`slot_erase`, `checkpoint_ring`, `tokenize`, `props`, `metrics`, `partial_state_only`, `context_shift`.
Alignment is attempted when `reachable && slots && (checkpoint_ring || slot_save)`.

---

## The three eviction profiles

The profile is **selected automatically from the capability probe**, and `SAKUR4_EVICTION_PROFILE`
overrides it. This is not tuning for its own sake — it is the correction of a measured mistake:

| Profile | Trigger | Target | Chosen when |
|---|---|---|---|
| **`cache-first`** | 75% | 55% | the backend exposes a checkpoint ring, so a preserved prefix is genuinely reusable |
| **`window-first`** | 70% | 30% | **no checkpoint source** — alignment is impossible, so window room is the scarcer resource |
| **`balanced`** | 75% | 45% | asked for explicitly |

Why the split exists, in the project's own numbers:

- The original **default** kept a large working set — evicting only to 55% — so a checkpoint-aligned
  boundary had room to exist. That is right on a backend with a ring, and **wrong on one without**:
  there is no checkpoint to align to, so the cost is paid and the benefit is not. Measured against a
  real llama.cpp exposing no checkpoints, that default cost **29% more tokens per turn and bought
  nothing**.
- Under `window-first` the reserve is **dropped** — keeping 4,096 tokens verbatim for a reuse the server
  will not confirm is pure cost (`cache_prefix_reserve_tokens` and `cache_prefix_max_tokens` both go to
  **0**).
- The result of choosing the profile from capabilities: the overhead came down to **+1.3%** while
  keeping **42 points of recall**. See [Benchmarks](Benchmarks).

The embedded backend simulates a checkpoint ring, so it selects `cache-first`; that is why an
`--backend embedded` run is a test of the mechanism and **not** evidence about your server.

---

## What `retained_prefix_tokens` means

`retained_prefix_tokens` is the number that makes the verdict checkable, and it is the field to read in
`context.plan_eviction` output. It is:

> the count of **live-window timeline tokens, measured from the head, that the plan leaves untouched
> verbatim** — computed by walking the episodes in order and stopping at the first one the plan
> escalated.

Three properties are worth stating, because each one is a correction:

- **It measures the outcome, not the aim.** Episodes are the unit of eviction, so the preserved prefix
  ends where an *episode* ends, which is rarely exactly where a checkpoint sits. An earlier version
  reported the boundary it aimed for, which "put a number in the receipt that no prompt ever had". The
  fix measures the result and revises the plan.
- **It uses the same walk that decided evictability.** The retained prefix is not recomputed by a second
  code path that might disagree; it comes from the same episode list and the same escalation set.
- **It is the shorter of two boundaries when they disagree.** If a checkpoint sits at 4,096 but the
  preserved prefix ends at 3,840, the plan reports **3,840** and says so: *"a checkpoint sits at
  {checkpoint_at} but the preserved prefix ends at {retained_prefix_tokens} tokens; the boundary is
  reported at the shorter position, which is what the next prompt will actually share."*

It is also what makes `partial-reuse` legitimate in the common case where the ring has moved past the
proposal: the boundary landed between the proposal and the oldest checkpoint, which is **normal**, and
the prefix up to the episode boundary is still a prefix of what the slot holds — so partial reuse is the
accurate verdict rather than a flattering one.

---

## On a backend with no checkpoint API

This is the case that was measured against a **real** llama.cpp server, and it is the most useful thing
on this page, because it is where the documentation and reality diverge in a way worth knowing before
you trust a receipt.

The server: a 27B model at Q3_K_XL, 81,920-token context, one slot, remote over a private network. The
probe result:

| Route | Status | Meaning |
|---|---|---|
| `/slots` | 200 | listing works, but the payload is thin: only `id`, `n_ctx`, `speculative`, `is_processing` |
| `/slots/0` | **404** | no per-slot detail route in this build |
| `/slots/0?action=save` | **501** | slot save is not implemented |
| `/slots/0?action=erase` | **501** | slot erase is not implemented |
| `/slots/0/checkpoints` | **404** | no checkpoint ring |
| `/tokenize` | 200 | exact tokenisation available |
| `/props` | 200 | advertises no checkpoint configuration |

Sakur4's diagnosis is **correct**, and the probe degraded gracefully rather than assuming:

```console
  capabilities     slots+tokenize
  coherence        no checkpoint source detected — compaction will report full re-prefill
```

### It is pessimistic, and that is the honest problem with it

**Explicit checkpoints are not the only mechanism.** llama.cpp reuses the longest common prefix of an
incoming prompt automatically. Measured on that server, holding the divergence position fixed and
growing the tail:

| New tokens after the cut | prompt tokens | reused | processed | reused % |
|---|---|---|---|---|
| 0 | 2,219 | 0 | 2,219 | 0% |
| 1 | 2,221 | 2,219 | 2 | **100%** |
| 2 | 2,227 | 2,221 | 6 | **100%** |
| 8 | 2,263 | 2,239 | 24 | 99% |
| 32 | 2,405 | 2,263 | 142 | 94% |
| 512 | 2,785 | 2,405 | 380 | 86% |

A **2,219-token preserved prefix followed by new content is reused in full.** That is precisely the shape
Sakur4's eviction produces, so the core mechanism is sound on that hardware — and the demo still reports:

```text
cache: FULL RE-PREFILL — compaction broke the prefix
no checkpoints are available on this backend/session; full re-prefill
0 tokens reused / 44705 prefilled (0% saved)
```

**Two distinct claims are being conflated in that message, and they should be separated:**

1. *"I cannot choose a cache-aligned boundary"* — **true** here, and unavoidable.
2. *"therefore you will pay a full re-prefill"* — **not true** here, and it is the number a user would
   act on. At the measured 0.89 ms/token, preserving a 4,000-token prefix is worth roughly **3.5
   seconds** per compaction; preserving 20,000 tokens is worth about **18 seconds**.

The fix the verification notes propose is a **third verdict alongside the existing four**:
`prefix-preserved, alignment unknown` — a prefix was kept, the backend exposes no checkpoint to confirm
it against, and LCP-based reuse is expected but unverified. **It is proposed, not implemented**: the
notes say so explicitly ("The honest fix is…"), and the status enum has no such variant. Until it
exists, the practical rule on a checkpoint-less backend is: **a `full-re-prefill` verdict there means
"unverifiable", not "nothing was reused"**.

One structural consequence is worth carrying into your own reasoning: a prompt that is a **strict
prefix** of what is resident reuses **nothing**, because there is no next-token position to evaluate.
Coherent rather than surprising — and it means **a compaction must leave a non-empty tail**.

`FR-7`'s row in the requirement table carries the same caveat in five words: *"not verifiable on a server
with no checkpoint ring"*.

---

## Three corrections worth recording

Each of these left **every existing test green** when it was wrong. They are now covered by contracts in
`crates/sakur4-core/tests/cache_coherence.rs`, which states the central claim as contracts and fails if
the preserved prefix stops being a **byte prefix** of what the server is actually sent.

**1. The order was inverted.** The first implementation chose evictions greedily from the oldest turn and
then asked the cache whether the resulting boundary happened to line up. It never did: the boundary
landed at token ~0, no checkpoint exists there, and every compaction reported a full re-prefill. Sakur4
had reproduced the exact failure it was written to remove, **using its own machinery**.

**2. A wrapped ring was treated as "nothing alignable".** A ring only spans `interval × depth` tokens, so
its oldest entry is usually well past a freshly-proposed boundary. Reporting a full re-prefill there is
wrong: the tokens **before** the oldest checkpoint are genuinely uncached, but everything **from it
onward** is reusable — and that is most of the prompt. The fix aligns forward onto the oldest checkpoint
when it is still early enough to leave a middle worth evicting.

**3. The plan reported the boundary it aimed for, not the one it produced.** See
[What `retained_prefix_tokens` means](#what-retained_prefix_tokens-means) — the receipt had a number no
prompt ever had.

A fourth, quieter issue: `PromptParts::timeline_tokens` estimated tokens as `chars / 4` while the receipt
**measured** them, so the eviction engine decided "relaxed" while the receipt printed a window
three-quarters full. Both now measure through the same tokenizer. This is the general rule in this
codebase: **every** budget decision and every printed number goes through one `TokenCounter`, because a
component that estimates while another measures has already caused a real bug here — twice.

---

## Partial-state architectures

Sliding-window and hybrid (attention + recurrent) models keep state a checkpoint cannot fully capture:
llama.cpp documents that on these the checkpoint holds only **part** of the context, so a rewind is not
guaranteed to reproduce the same outputs.

`partial_state_only` is detected by matching **architecture substrings in the model path**: `swa`,
`sliding`, `gemma3`, `gemma-3`, `recurrent`, `mamba`, `hybrid`, `jamba`, `qwen3next`, `lfm2`, `granite`.
When it fires:

- only **durable save points** are trusted for alignment — `slot_save_file`, `pre_rewrite`, `fold_marker`;
- a **ring rewind is not attempted**, because it is not guaranteed to reproduce the same outputs;
- if no durable save point exists, the plan is a full rewrite with that reason attached, which is the
  safe path rather than a silent approximation.

There is a related capability, `context_shift`, for servers that switch slots on prefix divergence (SWA
or hybrid handling), where an LCP mismatch may reset the whole slot.

### Snapshot and restore

When a rewrite is genuinely unavoidable, the pre-rewrite state is persisted first, so the expensive
prefill is **never lost** even when it cannot be reused — "the point is not that the old state will be
restored, usually it will not, but that recovering it never requires paying for the prefill twice."

| Setting | Default | Note |
|---|---|---|
| `snapshot_before_rewrite` | `true` | taken when a rewrite cannot be aligned |
| `snapshot_even_when_aligned` | `false` | a **slot-save file is 60–500 MB**, so it is not free |
| `snapshot_retention_per_slot` | 4 | older files are pruned by count |
| `min_free_disk_bytes` | 2 GiB | a snapshot is skipped, with a warning, below this |

`memory.fold` uses the same machinery: it takes a save point **at open**, records the token position as
a checkpoint, and on unfold collapses the tagged episodes to `referenced`, commits **one** summary
episode, and rolls the slot back to the pre-fold checkpoint — preferring an in-memory ring rewind,
falling back to a slot restore, and **reporting honestly when neither was possible**.

---

## The hosted-provider half

A hosted provider has no slot API. But it does report, in every response, how many prompt tokens came
from its prompt cache — and that is enough to do the same accounting:

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

The signature is an **inversion**: append-only growth makes the cached prefix *grow*, while a rewrite
that replaces a long prefix with a shorter one makes it *shrink* even as the prompt stays large. That
inversion is detectable, and `context.receipt` reports it per session.

Field names differ by provider — OpenAI's `prompt_tokens_details.cached_tokens`, Anthropic's
`cache_read_input_tokens`, DeepSeek's `prompt_cache_hit_tokens`, Gemini's `cachedContentTokenCount` —
and on providers where the figure is often absent, the honest move is to **omit the flag rather than
send zero**. Omitting is not the same as zero: zero asserts a cache miss, while omitting says the
provider did not report one, and Sakur4 says so rather than blaming a cache it cannot see.

**Sakur4 never calls a provider itself.** It is a subsystem, not a harness — the same reason it does not
call the model. The harness pushes the numbers; Sakur4 does the accounting and the eviction.

---

## What is not established

- **The exact reuse rule is not fully reverse-engineered.** Some measurements show zero reuse where
  prefix matching alone predicts a hit — for instance a shortened prompt sent immediately after a longer
  one. Whether that is request ordering, a per-request checkpoint, or a heuristic is **unresolved**.
  Everything reported above was reproduced, but the boundary conditions are not pinned down.
- **The measurements are deterministic, synthetic prompts**, on **one model, one build, one machine**.
  Real agent transcripts contain more repetition, which may tokenise and cache differently, and the
  server's `--cache-reuse` / `--ctx-checkpoints` flags were not inspected — only the HTTP surface was.
  Generation cost was excluded (`n_predict = 1`), which isolates prefill but is not a whole-turn
  measurement.
- **No LoCoMo and no Endurance Benchmark were run.** The A/B in `docs/bench/` is narrower: matched
  windows, one repository, one model. See [Benchmarks](Benchmarks).
- **A pipelined batch can reorder a read against a write.** Not a cache-coherence defect, but it is the
  defect that makes a compaction look broken from the outside, and it is open. See
  [Limitations](Limitations).

---

## Where to go next

| If you want… | Read |
|---|---|
| The tools that report these verdicts, with arguments | [Tool Reference](Tool-Reference) |
| Where the boundary sits in the wider system | [Architecture](Architecture) |
| The measured A/B behind the profile numbers | [Benchmarks](Benchmarks) |
| Every open defect, stated plainly | [Limitations](Limitations) |

---

<sub>[← Back to Home](Home) · [All pages](Home#where-to-go-next)</sub>
