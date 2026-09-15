# A/B benchmark: what actually changes

The question this answers: **run the same long session with and without Sakur4, and
what differs?** Not a microbenchmark — the whole session, including compaction and
memory.

```bash
node docs/bench/ab.mjs --repo . --turns 200 --tokens-per-turn 900
node docs/bench/ab.mjs --repo . --backend http://your-llama-server:8080
```

## The result, first, because it is not the flattering one

205 turns against a real repository, a 32,768-token window, two arms:

| | without Sakur4 | with Sakur4 | change |
|---|---|---|---|
| peak context tokens | 24,443 | 25,415 | **+4%** |
| total tokens sent | 2,807,158 | 3,634,256 | **+29%** |
| compactions | 6 | 25 | **+317%** |
| prefix tokens preserved | 0 | 519,504 | — |
| **recall accuracy** | **17%** | **92%** | **+75 pts** |
| **pinned constraint survived** | **lost at compaction 1** | **survived all 25** | — |

**Sakur4 costs more tokens and buys correctness with them.** That is the honest
summary, and it is not what a project README usually claims.

## Why it costs more

Anchors and retrieval are *not* the cost. Across the whole run they account for about
2,400 tokens out of 827,098 — 0.3%. The cost is somewhere else, and it is worth being
precise about because it is a design choice rather than an inefficiency:

- **The "without" arm keeps 30% of the transcript when it compacts**, so it sheds most
  of its history in six large, infrequent compactions.
- **Sakur4 evicts to a 55% target and triggers at 75%**, so it compacts more often and
  holds its working set higher.

Both are reasonable. They optimise different things: the first minimises tokens, the
second keeps a predictable working set and never lets history overflow. But the token
bill follows from that choice, and a user should know it before adopting.

## Why recall is so lopsided, and why that number needs a caveat

17% against 92% is a large gap, and the mechanism is simple: **once the naive arm
compacts, the planted facts are gone.** It scored 2/12. Sakur4 retrieves them from an
append-only store that nothing ever rewrites.

The caveat: this is a *scripted* workload, so it measures the memory layer, not a
model's judgement about when to consult it. A live model with a good `grep` could
recover some of those facts from the repository itself — the planted facts here are not
all things `grep` would find, but some are. **A live-model comparison is a different
experiment with different variance**, and this script is not a substitute for one.

## The constraint result is the cleanest finding

The naive arm planted `never force-push to main` at turn 1 and **destroyed it at its
first compaction** — turn ~40 of 205. It was not degraded gradually; it was gone.

Sakur4's arm survived all 25 compactions, because anchors are not eviction candidates:
the engine selects from episodes, and anchors live in a different table. The operation
of evicting a pinned constraint is not expressible.

That is a structural guarantee rather than a statistical one, and it is the result here
most likely to transfer to a real session.

## What is *not* established

- **The prefill saving is conditional.** 519,504 tokens of preserved prefix were kept
  reusable, worth roughly 462 s at the 0.89 ms/token measured against your llama.cpp —
  but *only if the backend reuses prefixes*. The default `embedded` backend simulates
  that rather than performing it, so the ledger deliberately does **not** total the two
  sides. Run with `--backend` for a real number.
- **Token counts are a calibrated heuristic.** One function measures both arms, so the
  comparison is sound; the absolute numbers are approximate.
- **The "without" arm models naive summarisation.** A harness that compacts well on its
  own would narrow the gap. OMP and Hermes have their own strategies, and the honest
  comparison is against those, not against a straw man.
- **No model was called.** Latency, task success, and whether a model *uses* the tools
  are all outside this measurement.

## Three bugs this benchmark had, recorded

A benchmark that lies quietly is worse than none, so:

1. **It measured nothing.** The first version used file excerpts small enough that
   neither arm ever crossed the compaction budget. It reported that Sakur4 cost 18%
   more — a true number about a situation that never happens in a long session. Fixed
   by padding turns to a target token count and running enough turns to force
   compaction in both arms.
2. **It compared the wrong quantities.** The `without` arm counted the assembled
   transcript; the `with` arm counted committed tokens. Those are not the same number,
   and the fix was to measure what each arm would actually send.
3. **It printed NaN as if it were a number.** A missing initialiser made "prefill
   avoided" read as `NaN`, and a percentage helper stripped the sign off regressions so
   a 33% *increase* displayed as `-33%`.

All three are visible in the script's comments rather than quietly patched, because the
next person to trust a number from here should know how the previous ones failed.

## Reproducing

```bash
# The default: embedded backend, heuristic token counts.
node docs/bench/ab.mjs --repo . --turns 200

# Against a real server, which is the only way the prefill numbers mean anything.
node docs/bench/ab.mjs --repo . --backend http://100.98.158.87:8080 --ms-per-token 0.89

# Smaller, quicker, still enough to compact.
node docs/bench/ab.mjs --repo . --turns 80 --tokens-per-turn 2000
```

Raw results are written to `--workdir/results.json` for anyone who wants to check the
arithmetic.
