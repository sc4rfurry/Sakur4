# Design Decisions

> The trade-offs behind Sakur4, **including the ones that were wrong first.** Most projects publish the
> decision; this page publishes the correction as well, because the correction is usually the part that
> generalises.

The full record is [`docs/DESIGN.md`](https://github.com/sc4rfurry/Sakur4/blob/master/docs/DESIGN.md),
which is long on purpose — it is the project's engineering log, not a summary of it.

---

## The big ones

### Eviction cuts at a cache boundary, not at a token count

**The decision.** When the context must shrink, choose a cut point the inference server's KV cache
already holds — the *latest* checkpoint at or before the requested cut, provided the gap is inside a
tolerance. Choosing "at or before" is what preserves the property that matters: the surviving prefix is
still a prefix of what the server has cached, so the next turn is a longest-common-prefix match rather
than a full re-prefill.

**Why it is the central bet.** A summarise-and-replace compaction throws away the cache *and* the text.
Evicting two thousand tokens out of a 200,000-token cached prefix costs a fraction of a re-prefill, and
on the measured workload the difference is the whole point.

**What it cost.** Real backends disagree about what they offer. The project measured one that returns
**501** for `?action=save` and has no checkpoint ring at all — so the honest answer there is
`no checkpoint source detected`, and the receipt becomes **pessimistic**: it reports `full-re-prefill`
where reuse is real but unverifiable. That is the PRD's top-rated risk occurring in the wild, and the
choice was to degrade visibly rather than to pretend.

### The profile follows the capability probe

**The decision.** Three eviction profiles — `CacheFirst` (75/55), `WindowFirst` (70/30), `Balanced`
(75/45) — and which one is used is decided by **what the backend can actually do**, not by a constant.
A user should not have to know their server lacks checkpoints in order to get the right ratios.

**Why.** With no checkpoint source, `CacheFirst` paid 29% more tokens per turn and bought nothing.
`SAKUR4_EVICTION_PROFILE` overrides when someone knows better, and that is honoured rather than
second-guessed.

### A model may not write to the Symbolic Ledger

**The decision.** Two memory tracks, with a hard wall between them. The Ledger holds symbols, signatures
and hashes extracted **deterministically by parsing** — no model output, so no hallucination. The Atlas
holds model-written summaries, and every entry must anchor to a Ledger fact.

**Why.** A memory layer whose facts come from the same model being helped is a memory layer that can
confidently remember something that never happened. Anchoring plus **read-time** staleness resolution is
what makes an Atlas entry falsifiable: a summary whose source has changed comes back labelled `[STALE]`
with the current value attached, rather than being quietly wrong.

**Note.** The Ledger is write-restricted structurally — triggers block `UPDATE` and `DELETE` — so this
is not a convention anyone can drift away from.

### Graduated eviction, one step at a time

**The decision.** Five tiers — `Live → Masked → Referenced → Archived → Dropped` — applied a rung at a
time across turns, with `Dropped` gated separately (it requires the episode to be marked droppable *and*
to have no live dependents).

**Why.** A binary drop loses information irreversibly at a moment nobody chose. Stepping down means a
turn that must leave the window **degrades** rather than vanishing, and every plan reports which of four
verdicts it reached.

**The correction.** The ladder originally refused *any* step that did not reclaim tokens — which
deadlocked it. A `Masked` stub renders a header plus a preview, so for a short episode the stub is
**longer** than the content it replaces: a 34-token message became an 80-token stub. Every candidate was
refused at the first rung, nothing ever reached `Referenced`, and a session could sit at 26,422 tokens of
pressure while the planner reported "would not reclaim anything at this tier" on every turn, forever.
The rule now refuses only a step at the **final** rung.

**And that correction caused a later bug**: the message that reports how many tokens a step reclaimed
subtracted the two counts, which underflows exactly in that case — a panic on an async worker, an
unanswered request, and a client that concluded the daemon was unreachable. Fixed with a saturating
subtraction, and the fix is guarded at the site.

### Refusal is visible, never silent

**The decision.** If the pinned anchors alone cannot fit the budget, the operation returns an **error**.
It does not drop one quietly.

**Why.** Pinned constraints are the one thing the preamble tells the model to rely on across compaction.
A guarantee that silently degrades is not a guarantee. This is FR-4's *"visible warning rather than silent
drop"*, and `render_anchor_block` is the only place it lives.

**The correction worth reading.** For the life of the project, **nothing called that function.** All four
paths that build a prompt assembled the anchor block themselves with a `join` — which enforces neither the
budget check nor the priority ordering. The unit tests passed throughout, because they tested the function
rather than any caller, and the gateway tests built anchor sets small enough that the `join` looked right.

### One store, several projects — and the transcript was the exception

**The decision.** `episodic_stream` gained a `project_id`, recall filters on it, and the status counts
are scoped to the caller's project.

**Why.** Reported from use: working in one project surfaced material from another. `episodic_stream` was
the one table with **no** `project_id` — everything else (`semantic_atlas`, `symbolic_fact`, `repo_file`,
`project`) was keyed by it — and it is the table the tools write on every turn.

**The correction.** The first migration left old rows `NULL` rather than backfilling from a guess. An old
episode's project is not recoverable, and attributing it to whichever project happened to be open at
migration time would reproduce the defect rather than fix it.

---

## Smaller ones, with the reasoning

| Decision | Why |
|---|---|
| **stdio by default, HTTP opt-in** | The common case is one harness spawning one process. HTTP exists for shared stores and concurrent clients, and is the only transport without a turnstile — it has no arrival order to preserve |
| **`sqlite-vec` optional** | The same binary gets in-database ANN where the extension exists and an exact scan where it does not. The MCP contract is identical either way |
| **`doctor` never fails on a bad store** | It is the command an operator reaches for *when something is wrong* — so it reports what it can rather than refusing |
| **The receipt records its own context window** | "12,446 tokens" means nothing without "of 32,768". The column was missing, so a stored receipt reported a window of zero and the pressure fraction was silently unavailable |
| **`serverInfo` deprecations are build failures** | CI runs clippy with `-D warnings`. A deprecation in a dependency is treated as something to fix now, which is how two `rmcp` types were migrated before a user hit them |
| **Unused dependencies fail the build** | Three crates were declared and referenced nowhere — including file-watching libraries that implied a feature the project did not have |

---

## What the corrections have in common

Reading back over them, the same shape recurs:

1. **A function exists, has a doc comment stating its purpose, and nothing calls it.** `open_folds`,
   `last_indexed`, `ImpactEntry.stale`, `render_anchor_block`, `is_backend_unavailable`,
   `EngineConfig::with_env`. Five of those six were a guarantee the project advertises and does not
   deliver.
2. **A guard that cannot fail.** Several checks passed while the thing they guarded was broken — an audit
   that read a fraction of each manifest, a scan that counted a struct field as a call, a link checker
   that walked into an untracked second checkout.
3. **A symptom that looks like a different problem.** A subtraction overflow presented as a dead daemon.
   A stale repository map presented as correct structure. A protocol-permitted reordering presented as a
   race.

The practice that came out of it, and the one this project would recommend to anyone: **when a check
passes, try to make it fail before trusting it.** Every guard in `docs/verification/` is expected to be
demonstrated failing on the thing it guards, and the ones that are not are marked as such.

---

<sub>[← Back to Home](Home) · [All pages](Home#where-to-go-next)</sub>
