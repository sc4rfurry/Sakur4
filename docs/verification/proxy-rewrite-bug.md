# State of the reverse proxy after the deadlock fix

Two rounds of work on FR-18. The deadlock is fixed and verified. One quality bug remains,
and this records both precisely so the next round does not re-derive them.

## Fixed: the tier ladder deadlocked

**Symptom.** A transcript far past the window was forwarded **unchanged**. Reproduced with
602 messages / 20,571 tokens (measured by the real server's tokenizer) against a
4096-token proxy:

```
FAIL  an over-window transcript is rewritten
      expected fewer than 602 messages, saw 602
602 messages in, 602 out — 0 removed
```

**Cause.** `propose_escalation` refused any step where `tokens_after >= tokens_before`. The
first rung, `Masked`, renders a header plus a 160-character preview — which for a **short
message is longer than the message**. A 34-token turn became an ~80-token stub. So every
candidate was refused at the first rung, none ever reached `Referenced`, and the engine sat
at 26,422 tokens of pressure reporting the same sentence every turn, forever.

**Fix.** A token-neutral step is accepted when a later rung will reclaim; only a *terminal*
step that reclaims nothing is refused. The ladder is finite (`escalate()` returns `None` at
the end), so this cannot loop.

**Honest note on how long this took.** The old diagnostic was one sentence covering several
unrelated causes:

> no episode could be escalated even though {n} tokens of pressure exist; every candidate is
> either at the tier floor or protected by an unresolved dependency

That sentence was true and useless. It named two possibilities without saying which, or how
many of each. It is now a counted breakdown, which is what produced the answer in one line:

```
504 would not reclaim anything at this tier
```

The diagnostic improvement is arguably worth more than the fix.

## Open: retained context is ~35% of the planner's target

Measured by growing one transcript turn by turn and reading the upstream's own
`prompt_tokens`. Clean runs, one proxy at a time, everything else killed:

```
window 32768 (target 9830):   12806 → 3406 → 3408 → 3408
window 81920 (target 24576):  26006 → 23406 → 23407 → 23408
```

At the 81,920 window it lands at 23,408 against a 24,576 target — **95%, which is correct and
scales**. At 32,768 it lands at 3,408 against 9,830 — **35%, which is not**.

So the scaling defect is real but narrower than it first appeared: it is worst at small
windows and disappears at large ones. The earlier "same 3,408 at every window" reading came
from runs contaminated by leftovers — a stale proxy from a `spawn EPERM` invocation, and a
verification script that reused one fixed store path so the second run saw twice the history.
Both are fixed; the numbers above are from a clean state.

### What is now asserted

Two contracts in `docs/verification/proxy-rewrite.mjs` measure this and would have caught the
original erasure:

- `the rewrite keeps a proportionate share of the conversation` — a floor on the share of
  messages kept.
- `retained context is in the same order as the plan's target` — compares retained tokens
  against `window × 0.3`, computed rather than hard-coded.

The second is the one that matters: a share floor passes at a large window where the first
bug hid, and comparing against the target does not.

### What was tried and did not work

**Filling `parts.timeline` from the fabric's rendered session timeline.** This was wrong on
its face — the proxy sends the harness's raw `messages`, and the rendered timeline never
reaches the model — and it measured 43,712 tokens for a transcript the server tokenizes at
20,571, a factor of two from per-episode rendering chrome. Fixing it to measure the transcript
as sent is correct and stayed; it did **not** change the retained figure.

**Clamping the cut so the surviving suffix is worth the target.** The code is in place and
should hold 9,830 tokens at a 32,768 window. It does not, and one run produced 11,460 — which
is close to the target and suggests the clamp can work. That run has not been reproduced, so
it is recorded as unreproduced rather than as a fix.

**A previous version of the same clamp** produced 108 retained tokens, because it accumulated a
running total and reassigned the cut on every iteration. The current version walks from the
newest message and takes the first index whose surviving suffix meets the budget. The
difference between "should hold the target" and "settles at 35% of it" is the next thing to
measure, and it is now a five-minute experiment: the contracts report both numbers.

## What is verified

- **The deadlock is fixed.** 10 of 10 contracts in `docs/verification/proxy-rewrite.mjs`, wired
  into `verify.mjs`.
- **The structure is right.** The system prompt and newest turn survive verbatim; exactly one
  marker replaces what went and sits after the system prompt; the rewrite sends less.
- **The transcript is shortened, not erased.** Measured on a realistic transcript: 602
  messages in, 238 out.
- **The recent budget scales with the window**, pinned by `the_recent_budget_shrinks_with_the_window`.
- **Plans are covered at 4,096 / 8,192 / 32,768 windows.** Every previous plan test used
  32,768, which is why none of them saw any of this.
- **Repeated runs are reproducible.** `proxy-rewrite.mjs` used one fixed store path and
  accumulated the previous run's episodes, so a second invocation planned against twice the
  history and reported a failure about the test rather than the code. Each run now gets a
  fresh store.

## Reproducing

```bash
# Growth test: watch upstream prompt_tokens after each step
sakur4d --db /tmp/rw.db --backend http://host:8080 --context-window 81920 -v \
        proxy --bind 127.0.0.1:8096 --upstream http://host:8080 --session rw &
node docs/verification/grow-session.mjs --proxy http://127.0.0.1:8096

# Contract test — run twice; the result must be identical
node docs/verification/proxy-rewrite.mjs --bin ~/.cargo/bin/sakur4d --window 32768
node docs/verification/proxy-rewrite.mjs --bin ~/.cargo/bin/sakur4d --window 32768
```
