# Benchmarks

> What was measured, by which method, and what the numbers do **not** establish — with every figure
> attributed to the experiment that produced it.

---

**On this page:** [Where each number comes from](#where-each-number-comes-from) ·
[The scripted A/B over 205 turns](#the-scripted-ab-over-205-turns) ·
[Why it is +1.3% and an earlier run said +29%](#why-it-is-13-and-an-earlier-run-said-29) ·
[The pinned constraint result](#the-pinned-constraint-result) ·
[Four bugs this benchmark had](#four-bugs-this-benchmark-had) ·
[Non-functional requirements, measured](#non-functional-requirements-measured) ·
[Recall latency at scale](#recall-latency-at-scale) ·
[Prefix reuse on real hardware](#prefix-reuse-on-real-hardware) ·
[The live-model A/B/C](#the-live-model-abc) ·
[What the method does not establish](#what-the-method-does-not-establish) ·
[Reproducing](#reproducing)

---

## Where each number comes from

Three different experiments are quoted on this site, and they answer different questions. Mixing them
up is the easiest way to misread this project's evidence, so the attribution is first:

| Experiment | Driver | Model in the loop | Turns | What it measures |
|---|---|---|---|---|
| **Scripted A/B** — `docs/bench/ab.mjs` | a script | **no** | **205** | tokens sent, compactions, recall mechanics, whether a pinned constraint survives |
| **Live-model A/B/C** — `docs/bench/live-model.md` | Oh My Pi 18.2.0 + `omp-sakur4` | **yes** (local 27B) | **3** (one question per arm) | whether a real model actually benefits |
| **Verification probes** — `docs/verification/` | plain Node / PowerShell | **no** (`n_predict = 1`) | n/a | the server's own prefix-reuse behaviour, and the NFR targets |

The Scripted A/B and the Live-Model A/B/C are **complementary and neither substitutes for the other**:
the script costs seconds and measures the engine, the live run costs **25–95 seconds per invocation** and
measures the model.

---

## The scripted A/B over 205 turns

The same session, run twice against a real repository: once the way a harness does today, and once
through Sakur4. Measured against a **real llama.cpp server** — a 27B model, an **81,920-token window**,
`window-first` selected automatically **because that server exposes no checkpoint API**:

| | without | with Sakur4 | change |
|---|---|---|---|
| tokens per turn | 31,580 | 31,980 | **+1.3%** |
| total tokens sent | 6,473,800 | 6,555,976 | +82,176 |
| compactions | 2 | 4 | +2 |
| peak context | 61,338 | 62,654 | +2% |
| prefix tokens kept reusable | 0 | 47,425 | — |
| **recall accuracy** | **50%** | **92%** | **+42 pts** |
| **pinned constraint survived** | **lost at compaction 2** | **survived** | — |

**+1.3% more tokens, for 42 points of recall and a constraint that no longer gets destroyed.** That is
the trade, and at this magnitude it is not a trade at all.

Why recall is lopsided, and the mechanism is simple: once the naive arm compacts, the planted facts go
with it. Sakur4 retrieves them from an append-only store that nothing rewrites.

```bash
node docs/bench/ab.mjs --repo .                              # embedded backend
node docs/bench/ab.mjs --repo . --backend http://host:8080   # real prefill numbers
```

The script **refuses to report** if either arm made zero compactions — see the bug list below.

---

## Why it is +1.3% and an earlier run said +29%

Two corrections, one in the benchmark and one in the engine, and both are worth knowing before you quote
any number here.

**The benchmark gave the arms different windows.** The first version used a 32,768-token window for both
arms while the daemon correctly resolved the server's real 81,920. The "without" arm therefore compacted
against 32K and the "with" arm against 82K — **the two arms were solving different problems**, and the
resulting **+29% was an artefact** of that mismatch. The benchmark now asks the daemon what window it
resolved and gives both arms the same one.

**The engine's default policy was wrong for a backend with no checkpoints.** The default kept a large
working set — evicting only to 55% — so a checkpoint-aligned boundary had room to exist. That is right on
a backend with a ring and **wrong on one without**: there is no checkpoint to align to, so the cost is
paid and the benefit is not. Measured against a real llama.cpp exposing no checkpoints, that cost **29%
more tokens per turn and bought nothing**.

The fix is that **the eviction profile is chosen from the backend's capabilities**, and it brought the
overhead to **+1.3% while keeping 42 points of recall**. `doctor` reports which profile is active and
why; `SAKUR4_EVICTION_PROFILE` overrides it.

| Profile | Trigger | Target | Chosen when |
|---|---|---|---|
| `cache-first` | 75% | 55% | the backend exposes a checkpoint ring, so a preserved prefix is reusable |
| `window-first` | 70% | 30% | no checkpoint source — alignment is impossible, so window room is the scarcer resource |
| `balanced` | 75% | 45% | asked for explicitly |

---

## The pinned constraint result

**The cleanest finding in the project.** The naive arm planted `never force-push to main` at turn 1 and
**destroyed it at its second compaction**. Not degraded gradually — **gone**.

Sakur4's arm survived, and the reason is structural rather than statistical: **anchors are not eviction
candidates.** The engine selects from episodes, and anchors live in a different table, so evicting a
pinned constraint is **not expressible**. That makes it the result most likely to transfer to a real
session, because it does not depend on a scoring heuristic being well tuned.

---

## Four bugs this benchmark had

A benchmark that lies quietly is worse than none, so all four are recorded in the script's own comments
rather than quietly patched:

1. **It measured nothing.** Turns were small enough that neither arm ever crossed the compaction budget,
   and it reported an **18% regression** for a situation that never happens in a long session. The script
   now refuses to report if either arm made zero compactions.
2. **It compared the wrong quantities.** The `without` arm counted the assembled transcript; the `with`
   arm counted committed tokens.
3. **It gave the arms different windows** — 32K for one, 82K for the other — producing the +29% above.
4. **It printed `NaN` as a number**, and its percentage helper stripped the sign, so a 33% *increase*
   displayed as `-33%`.

---

## Non-functional requirements, measured

Run on the **development machine**, not the PRD's reference hardware — the development box has a GPU with
4 GB, which cannot host the target workload at all, which is also why the embedded and fake backends
exist.

| NFR | Target | Measured | Verdict |
|---|---|---|---|
| **NFR-1** incremental re-index, 10,000 files | < 2 s | **1.55 s** (full index 14.5 s, store 78.6 MB) | pass, **tight** |
| **NFR-2** `memory.recall` at 100,000 entries | < 300 ms | **88 ms** worst p95 | pass |
| **NFR-3** boundary decision overhead | < 50 ms | **71 ms** including process start and store open | **inconclusive** |
| **NFR-4** idle RSS | < 200 MB | **7.7 MB** | pass |
| **NFR-5/6** transactional writes, crash-safe resume | no corruption | `a_restart_preserves_the_store_and_resumes` | pass |
| **NFR-7** graceful degradation with no checkpoint API | never a hard failure | confirmed against a real server | pass |

**NFR-3 is not properly measured, and the verdict says so.** The 71 ms is a whole `sakur4d plan`
invocation — process spawn, store open, engine construction, *and* the decision. The requirement is about
the decision's overhead **inside a running session**, which needs an in-process benchmark rather than a
CLI invocation. The honest reading is that the budget is not obviously exceeded, **not** that it is met.

**NFR-1 passes by 0.45 s.** Worth stating rather than rounding up.

---

## Recall latency at scale

NFR-2 at full scale, by query class. The distribution matters more than the mean: a term present in every
episode exercises the ranker, while a unique token exercises only the index, and an average would hide
whichever is slow.

```text
  query class            min     median      p95      max
  ubiquitous term          59ms        62ms       65ms       65ms
  mid-frequency term       61ms        68ms       88ms       88ms
  unique token              2ms         2ms        2ms        2ms
  unique marker             2ms         2ms        5ms        5ms
  two-term phrase           3ms         3ms       78ms       78ms
  absent term               1ms         1ms        2ms        2ms
```

```bash
node docs/verification/nfr2-recall.mjs --n 100000     # ~5 minutes of seeding
```

The worst case — **88 ms p95**, a mid-frequency term, against the 300 ms target — is where the ranker
does real work. The 2 ms rows are index lookups and are not evidence that ranking is cheap.

---

## Prefix reuse on real hardware

These come from `docs/verification/`, against a real llama.cpp server (27B at Q3_K_XL, 81,920-token
context, one slot), with `n_predict = 1` to isolate prefill from generation.

**The server has no checkpoint API.** The probe: `/slots` 200 but a thin payload, `/slots/0` **404**,
`/slots/0?action=save` **501**, `?action=erase` **501**, `/slots/0/checkpoints` **404**, `/tokenize`
200, `/props` 200 with no checkpoint configuration.

**Prefix reuse works anyway, and it is large.** Cold vs. identical vs. head-changed, at a 3,045-token
prompt:

| Case | prompt processed | cache reused | wall clock |
|---|---|---|---|
| A — cold | 3,045 | 0 | **2,743 ms** |
| B — identical prompt again | 4 | 3,041 | **162 ms** |
| C — same prompt, head changed | 3,060 | 0 | **2,426 ms** |

- **Prefill costs 0.89 ms/token** — about **1,118 tokens/second** on a 27B at Q3.
- **Re-sending an identical prompt is 94% faster.**
- **Changing the head costs 15–17×** what the cached version costs.
- Reproduced on a second run at 2,017 tokens: cold 1,853 ms, identical 159 ms, head changed 1,704 ms —
  the same shape at **0.92 ms/token**.

**The case Sakur4 actually produces** — holding the divergence position and growing the tail:

| New tokens after the cut | prompt tokens | reused | processed | reused % |
|---|---|---|---|---|
| 0 | 2,219 | 0 | 2,219 | 0% |
| 1 | 2,221 | 2,219 | 2 | **100%** |
| 2 | 2,227 | 2,221 | 6 | **100%** |
| 8 | 2,263 | 2,239 | 24 | 99% |
| 32 | 2,405 | 2,263 | 142 | 94% |
| 512 | 2,785 | 2,405 | 380 | 86% |

A **2,219-token preserved prefix followed by new content is reused in full** — precisely the shape
Sakur4's eviction produces. The `0`-new-token row is coherent rather than surprising: a prompt that is a
strict prefix of what is resident reuses nothing, because there is no next-token position to evaluate.
A compaction must therefore leave a **non-empty tail**.

**And the receipt is pessimistic on this backend.** The same server produces
`no checkpoint source detected`, and `sakur4d demo` reports `FULL RE-PREFILL … 0 tokens reused / 44705
prefilled (0% saved)` — where reuse is real but unverifiable. At 0.89 ms/token, preserving a 4,000-token
prefix is worth roughly **3.5 seconds** per compaction and 20,000 tokens about **18 seconds**. The full
accounting, and the third verdict the notes propose for it, is on [Cache Coherence](Cache-Coherence).

**The saving in the A/B ledger is conditional, and the ledger says so.** The **47,425** tokens of
preserved prefix were kept reusable, worth roughly **42 s** at 0.89 ms/token — but *only if the backend
reuses prefixes*. The ledger deliberately does **not** total the two sides, because the extra tokens are
certain and the saving is not.

---

## The live-model A/B/C

Every other measurement here exercises the engine. This one asks whether a **model** benefits — and
whether the plumbing that was supposed to deliver the memory actually delivers it.

| | |
|---|---|
| Harness | Oh My Pi 18.2.0, the `omp-sakur4` extension |
| Model | a locally served 27B at Q3_K, 81,920-token window |
| Repo | a three-file fixture whose contents deliberately say **nothing** about the question |
| Question | *"What is the one rule about the migrations directory in this project? Answer in one short line, or say UNKNOWN."* |

The fixture matters: if the answer were discoverable by reading the repository, a correct answer would
prove nothing. Its `README.md` says in as many words that nothing there mentions migrations — so the only
possible source of a correct answer is Sakur4.

**On the harness version, which is worth a caveat of its own.** The README records that this single fact
was previously written at **three different values** — 18.1.17, 18.2.0 and 18.2.11 — and that the plugin
declares **no version floor at all**: its `package.json` carries
`"@earendil-works/pi-coding-agent": "*"` as an optional peer dependency, so the documented minimum is
prose rather than an enforced constraint. The extension API is undocumented, so a minor bump can change
it without notice; `--no-extensions` is how to tell whether a failure is OMP's or this plugin's.

| Arm | Plugin | Store | Answer |
|---|---|---|---|
| **A** | active | holds the constraint | **"Migrations are generated by `scripts/gen.sh` — never edit anything under `migrations/` directly."** |
| **B** | **disabled** (`--no-extensions`) | holds the constraint (same store) | UNKNOWN |
| **C** | active | **empty** | UNKNOWN |

**Arm B is the control that makes this meaningful**: same populated store as arm A, plugin off, wrong
answer. Arm C isolates the other variable: plugin on, nothing to inject, UNKNOWN again. The correct answer
is attributable to the plugin, to the store contents, and to **both being present**.

**Why arm A should have failed, which is the point of running it.** Until round 2 of that work, the
extension **never injected the Anchor Set** — its context hook called `memory.recall` and nothing else. A
short rule like *"never edit anything under `migrations/`"* matches almost no query, so it was absent from
most turns. Arm A is the same experiment that would have produced **UNKNOWN in all three arms** before
that fix. The bug was in the live path only: the engine kept anchors out of eviction correctly, and the
claim that pinned content is rendered verbatim into every prompt was true of the engine and false of the
only place a user can see it.

**Does the model use the tools? Measured: no, not when the hook suffices.** Asking the model to search
its memory explicitly returned a uniquely-named fact correctly in 50.1s, but that run could not
distinguish the model **choosing** to call the tool from the hook **injecting** the fact. The extension
now counts both:

```text
session summary {"toolCalls":0,"retrievals":1}
```

The model did not call the tool. It answered from what the context hook had already injected. Both paths
produce the same user-visible result and they are **not the same finding**:

- `retrievals > 0, toolCalls == 0` — the hook supplied the context and the model used it. **This is the
  intended design**: the point of injecting is that the model should not have to know it needs to search.
- `toolCalls > 0` — the model decided on its own that memory was missing something. That is what
  `sakur4_recall` exists for, and it is the case the hook cannot cover, because the hook only injects what
  the *current prompt* resembles.

**Cost is why the arms are short.** Each OMP invocation against this server costs **25–95 seconds** — the
harness sends a large system prompt and the local model prefills at roughly **1,100 tokens/second**. A
20-turn A/B would take about **45 minutes per arm** and would add little: the question above is decided in
one turn. That is also why `ab.mjs` uses a scripted driver.

---

## What the method does not establish

Stated plainly, because the alternative is finding out later.

**About the scripted A/B:**

- **One repository, one model, matched windows.** The benchmark asks the daemon what window it resolved
  and gives both arms the same one, which is a fix for a real bug — but it is still one repository and one
  model.
- **Token counts are a calibrated heuristic** — factor **0.905** against the real tokenizer. One function
  measures **both** arms, so the *comparison* is sound; the absolute numbers are approximate.
- **The "without" arm models naive summarisation.** A harness that compacts well on its own would narrow
  the gap. OMP and Hermes have their own strategies, and the honest comparison is against those.
- **No model was called.** Latency, task success, and whether a model *uses* the tools are all outside
  this measurement. It measures the memory layer, not a model's judgement about when to consult it. A live
  model with a good `grep` could recover some of the planted facts from the repository itself, which is
  why the live-model fixture was built to make that impossible.

**About the live-model run:**

- **No multi-turn session has been run.** These are single-turn probes. Whether the model behaves
  differently at turn 100, after several compactions, is **untested**.
- **Model-initiated tool use is unproven.** In the one case measured it did not call the tool, because
  injection had already answered the question. Whether it calls `sakur4_recall` when the hook *cannot* help
  is untested.
- **Hermes has never been driven by a live model** — only its transport and its ContextEngine interface.
  The OMP extension is the only harness with a live-model result.

**About the numbers generally:**

- **No LoCoMo. No Endurance Benchmark.** Neither was run. The A/B here is a different and narrower
  measurement, and it **does not substitute** for a standardised long-conversation benchmark.
- **The NFR figures are from the development machine**, not the PRD's reference hardware. NFR latency and
  memory numbers are unmeasured on reference hardware.
- **NFR-3 is inconclusive**, for the reason given above rather than because it failed.
- **The prefix-reuse measurements are deterministic synthetic prompts** on one model, one build, one
  machine, with generation excluded. The exact reuse rule is not fully reverse-engineered: some
  measurements show zero reuse where prefix matching alone predicts a hit, and that is **unresolved**.

---

## Reproducing

```bash
# The scripted A/B. The default is the embedded backend (which SIMULATES checkpoints,
# so cache-first) — a real server is the only way the numbers here mean anything.
node docs/bench/ab.mjs --repo . --turns 200 --tokens-per-turn 900
node docs/bench/ab.mjs --repo . --backend http://your-llama-server:8080 --ms-per-token 0.89

# Force a profile, to compare tunings on the same backend.
SAKUR4_EVICTION_PROFILE=cache-first node docs/bench/ab.mjs --repo . --backend http://...

# Recall latency at scale.
node docs/verification/nfr2-recall.mjs --n 100000 --bin ~/.cargo/bin/sakur4d

# The live-model A/B/C (needs OMP and a local model).
# See docs/bench/live-model.md — the fixture and the three arms, in full.
```

Raw A/B results are written to `--workdir/results.json` for anyone who wants to check the arithmetic.
The live-model protocol is in
[`docs/bench/live-model.md`](https://github.com/sc4rfurry/Sakur4/blob/master/docs/bench/live-model.md),
and the NFR scripts are in
[`docs/verification/README.md`](https://github.com/sc4rfurry/Sakur4/blob/master/docs/verification/README.md).

---

## Where to go next

| If you want… | Read |
|---|---|
| Why preserving a prefix is worth anything | [Cache Coherence](Cache-Coherence) |
| How each check is run, and what it needs | [Verification](Verification) |
| Every open defect, stated plainly | [Limitations](Limitations) |
| The tools that produced these numbers | [Tool Reference](Tool-Reference) |

---

<sub>[← Back to Home](Home) · [All pages](Home#where-to-go-next)</sub>
