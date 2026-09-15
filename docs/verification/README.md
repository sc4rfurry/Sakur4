# Verification against a real llama.cpp server

Everything in this directory talks to a **real** server over HTTP. Nothing here is a
mock. This is the record of what was measured, what it establishes, and — as
importantly — what it does not.

## The server under test

| | |
|---|---|
| Endpoint | `http://your-llama-server:8080` (remote, reached over a private network) |
| Model | `a 27B model at Q3_K_XL`, alias `a 27B model` |
| Build | `the current build` |
| Context | 81,920 tokens |
| Slots | 1 |
| Platform | Windows (from the model path) |

## Headline results

### The capability probe — Sakur4's diagnosis was correct

| Route | Status | Meaning |
|---|---|---|
| `/slots` | 200 | slot listing works, but the payload is thin: only `id`, `n_ctx`, `speculative`, `is_processing` |
| `/slots/0` | **404** | no per-slot detail route in this build |
| `/slots/0?action=save` | **501** | slot save is not implemented |
| `/slots/0?action=erase` | **501** | slot erase is not implemented |
| `/slots/0/checkpoints` | **404** | no checkpoint ring |
| `/tokenize` | 200 | exact tokenisation available |
| `/props` | 200 | advertises no checkpoint configuration |

`sakur4d doctor` against this server reports:

```
capabilities     slots+tokenize
coherence        no checkpoint source detected — compaction will report full re-prefill
```

That is correct, and it is the PRD's top-rated risk — *"llama.cpp's slot/checkpoint API
is a moving target"* — occurring in the wild. The probe degraded gracefully rather than
assuming, which is exactly what NFR-7 asks for.

### Prefix reuse works anyway, and it is large

Explicit checkpoints are not the only mechanism. llama.cpp caches the longest common
prefix of an incoming prompt automatically. Measured on this server with a 3,045-token
prompt, `n_predict = 1`, `cache_prompt = true`:

| Case | prompt processed | cache reused | wall clock |
|---|---|---|---|
| A — cold | 3,045 | 0 | **2,743 ms** |
| B — identical prompt again | 4 | 3,041 | **162 ms** |
| C — same prompt, head changed | 3,060 | 0 | **2,426 ms** |

Reproduced on a second run at a smaller prompt size (2,017 tokens): cold 1,853 ms,
identical resend 159 ms, head changed 1,704 ms — the same shape at 0.92 ms/token.
The numbers are stable across runs, not a single lucky measurement.

- **Prefill costs 0.89 ms/token** — about **1,118 tokens/second** on a 27B at Q3.
- **Re-sending an identical prompt is 94% faster**, and the server reports only 4 tokens
  processed.
- **Changing the head costs 15–17×** what the cached version costs. That is the failure
  Sakur4 exists to prevent, reproduced on real hardware rather than in a simulation.

### The case Sakur4 actually produces

Reuse requires *new material after the shared prefix*. Holding the divergence position
fixed and growing the tail (`reuse-rule.mjs`, section A):

| New tokens after the cut | prompt tokens | reused | processed | reused % |
|---|---|---|---|---|
| 0 | 2,219 | 0 | 2,219 | 0% |
| 1 | 2,221 | 2,219 | 2 | **100%** |
| 2 | 2,227 | 2,221 | 6 | **100%** |
| 8 | 2,263 | 2,239 | 24 | 99% |
| 32 | 2,405 | 2,263 | 142 | 94% |
| 512 | 2,785 | 2,405 | 380 | 86% |

A 2,219-token preserved prefix followed by new content is reused **in full**. That is
precisely the shape Sakur4's eviction produces, so its core mechanism is sound on this
hardware.

A prompt that is a *strict prefix* of what is resident — nothing new after the cut —
reuses nothing, because there is no next-token position to evaluate. That is coherent
rather than surprising, and it means a compaction must leave a non-empty tail.

## What this changes about Sakur4

**The receipt is pessimistic on this backend.** `sakur4d demo --backend
http://your-llama-server:8080` reports:

```
cache: FULL RE-PREFILL — compaction broke the prefix
no checkpoints are available on this backend/session; full re-prefill
0 tokens reused / 44705 prefilled (0% saved)
```

The *diagnosis* is right — there is no checkpoint API to align to. The *prediction* is
wrong: llama.cpp will reuse a preserved prefix without any checkpoint, so the actual
saving is not zero. At the measured 0.89 ms/token, preserving a 4,000-token prefix is
worth roughly **3.5 seconds** per compaction; preserving 20,000 tokens is worth about
**18 seconds**.

Two distinct claims are being conflated in that message, and they should be separated:

1. *"I cannot choose a cache-aligned boundary"* — true here, and unavoidable.
2. *"therefore you will pay a full re-prefill"* — not true here, and it is the number a
   user would act on.

The honest fix is a third verdict alongside the existing four: **`prefix-preserved,
alignment unknown`** — we kept a prefix, the backend exposes no checkpoint to confirm it
against, and LCP-based reuse is expected but unverified. That is what the evidence
supports and no more.

## What is *not* established

Stated so nobody mistakes this for more than it is.

- **The exact reuse rule is not fully reverse-engineered.** Some measurements show zero
  reuse in cases where prefix matching alone predicts a hit — for instance a shortened
  prompt sent immediately after a longer one. Whether that is request ordering, a
  per-request checkpoint, or a heuristic is unresolved. Everything reported above was
  reproduced, but the boundary conditions are not pinned down.
- **These are deterministic, synthetic prompts.** Real agent transcripts contain more
  repetition, which may tokenise differently and cache differently.
- **One model, one build, one machine.** `the current build` is not every build, and the server's
  `--cache-reuse` and `--ctx-checkpoints` flags were not inspected — only the HTTP
  surface was.
- **`n_predict = 1`, so generation cost is excluded** from the timings. That is
  deliberate: it isolates prefill, but it is not a whole-turn measurement.
- **The earlier scripts in this directory contain recorded failures** — a prompt
  generator that produced 156,126 tokens from a request for 3,000, and a variant sweep
  whose prompts silently overflowed the context window. Both are documented in the
  files themselves, because a measurement script that lies quietly is worse than no
  script.

## Running these

```bash
BASE=http://host:port

# Headline numbers: cold vs cached vs head-changed.
node docs/verification/llamacpp-prefix.mjs --base $BASE --tokens 3000

# Reuse against tail size and divergence position, one variable at a time.
node docs/verification/reuse-rule.mjs --base $BASE

# What lengthening and shortening a prompt do.
node docs/verification/shortening-probe.mjs --base $BASE

# Is a short run of characters a token boundary? (No.)
node docs/verification/lcp-diagnose.mjs --base $BASE
```

All four are dependency-free Node scripts using `fetch`. They only require the server
to be reachable; none of them need Sakur4 itself.
