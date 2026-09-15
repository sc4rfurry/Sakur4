# A/B benchmark: what actually changes

The question: **run the same long session with and without Sakur4, and what differs?**

```bash
node docs/bench/ab.mjs --repo . --turns 200 --tokens-per-turn 900
node docs/bench/ab.mjs --repo . --backend http://your-llama-server:8080 --ms-per-token 0.89
```

## The result, against a real llama.cpp server

205 turns, a real repository, an 81,920-token window, `window-first` profile selected
automatically because that server exposes no checkpoint API:

| | without | with Sakur4 | change |
|---|---|---|---|
| tokens per turn | 31,580 | 31,980 | **+1.3%** |
| total tokens sent | 6,473,800 | 6,555,976 | +82,176 |
| compactions | 2 | 4 | +2 |
| peak context | 61,338 | 62,654 | +2% |
| prefix tokens kept reusable | 0 | 47,425 | — |
| **recall accuracy** | **50%** | **92%** | **+42 pts** |
| **pinned constraint survived** | **lost at compaction 2** | **survived** | — |

**+1.3% more tokens, for 42 points of recall and a constraint that no longer gets
destroyed.** That is the trade, and at this magnitude it is not a trade at all.

## Why this number is +1.3% and an earlier run said +29%

The first benchmark used a 32,768-token window for both arms while the daemon correctly
resolved the server's real 81,920. The "without" arm therefore compacted against 32K and
the "with" arm against 82K — **the two arms were solving different problems**, and the
resulting +29% was an artefact of that mismatch. The benchmark now asks the daemon what
window it resolved and gives both arms the same one. Details below.

Two further corrections, both in the engine rather than the benchmark:

1. **The eviction profile is now chosen from the backend's capabilities.** The default
   policy keeps a large working set (evicts only to 55%) so a checkpoint-aligned
   boundary has room to exist. That is right on a backend with a checkpoint ring and
   wrong on one without: there is no checkpoint to align to, so the cost is paid and the
   benefit is not. A backend with no checkpoint source now gets `window-first`
   (trigger 70%, target 30%), which sheds as a naive harness does — because it has no
   cache-alignment reason not to.
2. **The reserve is dropped under `window-first`.** Keeping 4,096 tokens verbatim for a
   reuse the server will not confirm is pure cost.

## The three profiles

Selected automatically from the capability probe. `SAKUR4_EVICTION_PROFILE` overrides.

| Profile | Trigger | Target | Chosen when |
|---|---|---|---|
| `cache-first` | 75% | 55% | the backend exposes a checkpoint ring, so a preserved prefix is reusable |
| `window-first` | 70% | 30% | no checkpoint source — alignment is impossible, so window room is the scarcer resource |
| `balanced` | 75% | 45% | asked for explicitly |

`doctor` reports which one is active and why:

```console
$ sakur4d --backend http://your-llama-server:8080 doctor
  capabilities     slots+tokenize
  coherence        no checkpoint source detected — compaction will report full re-pre-fill
  eviction         window-first · trigger 70% · target 30% · keep 4096 recent
```

## Why recall is lopsided, and why that number needs a caveat

50% against 92% — and the mechanism is simple: once the naive arm compacts, the planted
facts go with it. Sakur4 retrieves them from an append-only store that nothing rewrites.

The caveat: this is a *scripted* workload, so it measures the memory layer, not a model's
judgement about when to consult it. A live model with a good `grep` could recover some of
these facts from the repository itself. **A live-model comparison is a different
experiment with different variance**, and this script is not a substitute for one.

## The constraint result is the cleanest finding

The naive arm planted `never force-push to main` at turn 1 and **destroyed it at its
second compaction**. Not degraded gradually — gone.

Sakur4's arm survived, because anchors are not eviction candidates: the engine selects
from episodes and anchors live in a different table. Evicting a pinned constraint is not
expressible. That is a structural guarantee rather than a statistical one, which makes it
the result most likely to transfer to a real session.

## What is *not* established

- **The prefill saving is conditional.** 47,425 tokens of preserved prefix were kept
  reusable, worth roughly 42 s at the 0.89 ms/token measured against the reference
  server — but *only if the backend reuses prefixes*. The ledger deliberately does not
  total the two sides, because the extra tokens are certain and the saving is not.
- **Token counts are a calibrated heuristic** (factor 0.905 against the real tokenizer).
  One function measures both arms, so the comparison is sound; the absolute numbers are
  approximate.
- **The "without" arm models naive summarisation.** A harness that compacts well on its
  own would narrow the gap. OMP and Hermes have their own strategies, and the honest
  comparison is against those.
- **No model was called.** Latency, task success, and whether a model *uses* the tools
  are all outside this measurement.

## Four bugs this benchmark had, recorded

A benchmark that lies quietly is worse than none, so:

1. **It measured nothing.** Turns were small enough that neither arm ever crossed the
   compaction budget. It reported an 18% regression for a situation that never happens
   in a long session. The script now *refuses to report* if either arm made zero
   compactions.
2. **It compared the wrong quantities.** The `without` arm counted the assembled
   transcript; the `with` arm counted committed tokens.
3. **It gave the arms different windows** — 32K for one, 82K for the other — which
   produced a +29% that was entirely an artefact. See above.
4. **It printed NaN as a number**, and its percentage helper stripped the sign, so a 33%
   *increase* displayed as `-33%`.

All four are visible in the script's comments rather than quietly patched, because the
next person to trust a number from here should know how the previous ones failed.

## Reproducing

```bash
# The default: embedded backend (which simulates checkpoints, so cache-first).
node docs/bench/ab.mjs --repo . --turns 200

# Against a real server, which is the only way the numbers above mean anything.
node docs/bench/ab.mjs --repo . --backend http://your-llama-server:8080 --ms-per-token 0.89

# Force a profile, to compare tunings on the same backend.
SAKUR4_EVICTION_PROFILE=cache-first node docs/bench/ab.mjs --repo . --backend http://...
```

Raw results are written to `--workdir/results.json` for anyone who wants to check the
arithmetic.
