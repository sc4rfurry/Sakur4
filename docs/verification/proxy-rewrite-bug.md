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

## Open: the retained size does not depend on the window

Measured by growing one transcript turn by turn and reading the upstream's reported
`prompt_tokens` after each step.

```
window  32768:  12806 → 3406 → 3408        (trimmed, then flat)
window  81920:  19411 → 39211 → 3412 → 3414
window 131072:  26006 → 52406 → 3408
```

**It settles at ~3,408 tokens at every window.** Three different windows, the same number to
within six tokens. A target that is a fraction of the window cannot produce that, so whatever
is bounding the result is not the target.

Note also *when* it fires: at 131,072 the transcript reached 52,406 tokens and was still
untouched, then collapsed to 3,408 on the next step. So the trim is a cliff, not a gradient —
one turn of history costs ~52,000 tokens and the next costs 3,408.

### What the plan says, measured

Instrumenting the planner directly, on 40,000 tokens of history at three windows:

```
window=32768  target=18022  savings=10560  after=29440  reaches_target=false  kept_tokens=8000
window= 8192  target= 4506  savings=10560  after=29440  reaches_target=false  kept_tokens=8000
window= 4096  target= 2253  savings=10560  after=29440  reaches_target=false  kept_tokens=8000
```

`target` scales correctly (18,022 / 4,506 / 2,253). `savings`, `after` and `kept_tokens` are
**identical** at all three. The planner escalates every eligible episode, reclaims 10,560
tokens when 21,978 were needed, misses its target, and reports so honestly through
`reaches_target()`.

### Fixed this round: `keep_recent_tokens` was absolute

The cause of the *constant* part is now identified and fixed. `keep_recent_tokens` is 4096 and
was used directly, so the same ~8,000 tokens of recent context were protected regardless of
the window. Over a 4,096-token window that reserves the entire budget, leaving no middle to
evict — which is the deadlock that made the proxy forward over-long transcripts untouched.

`EvictionPolicy::recent_budget_for(window)` now treats it as an upper bound and shrinks it to
at most a quarter of the window, with a 512-token floor. Behaviour at 32k and above is
unchanged. Pinned by `the_recent_budget_shrinks_with_the_window`, and the retained set does
now scale at the planner level (8,000 → 6,000 → 5,000 tokens kept).

### Still open: the proxy rewrites to ~3,408 regardless

The planner-level fix did not change the field result, so the binding constraint is elsewhere.
The numbers to reconcile: the proxy reports `live_tokens: 43,712` for a transcript whose text
the server tokenizes at 20,571 — a factor of about two — and `needed` is computed from the
former. If `live_tokens` double-counts, the planner is aiming at a target derived from roughly
twice the real transcript, which would explain both the cliff and the constant.

**That is the next measurement**: compare `plan.live_tokens` against the server's own
`prompt_tokens` for the same request. One number, and it settles whether the input to the
planner is wrong or the planner's application of it is.

### A fix attempted and reverted

Bounding the proxy's cut so the retained tail is worth at least `target` produced a retained
prompt of **108 tokens** — worse than the bug it was meant to fix. The backward walk measured
plain message text while the plan's target is in rendered-timeline tokens, and the two are not
comparable. Reverted rather than debugged in place: the planner already aims at the target, and
a second, differently-measured bound fighting it is how a fix becomes two bugs. The reasoning is
recorded in the code so the next attempt does not repeat it.

## What is verified

- **The deadlock is fixed.** 8 of 8 contracts in `docs/verification/proxy-rewrite.mjs`, wired
  into `verify.mjs` so it runs every time.
- **The structure is right.** The system prompt and newest turn survive; exactly one marker
  replaces what went, after the system prompt; the rewrite sends less.
- **Proportionality is asserted**: `some conversation survives the rewrite`.
- **Plans are covered at 4,096 / 8,192 / 32,768 windows.** Every previous plan test used
  32,768, which is why none of them saw any of this.
- **The recent budget scales with the window**, pinned by its own test.

## Reproducing

```bash
# Growth test: watch upstream prompt_tokens after each step
sakur4d --db /tmp/rw.db --backend http://host:8080 --context-window 81920 -v \
        proxy --bind 127.0.0.1:8096 --upstream http://host:8080 --session rw &
node docs/verification/grow-session.mjs --proxy http://127.0.0.1:8096

# Contract test
node docs/verification/proxy-rewrite.mjs --bin ~/.cargo/bin/sakur4d --window 32768
```
