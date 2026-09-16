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

## Open: the plan overshoots its own target, badly

Measured by growing one transcript turn by turn and reading the upstream's reported
`prompt_tokens` after each step.

At a 32,768-token proxy, `window-first` profile (trigger 70% = 22,938, target 30% = 9,830):

```
sent 202 msgs -> upstream prompt_tokens 12811
sent 402 msgs -> upstream prompt_tokens  3411   ← rewrite fires
sent 602 msgs -> upstream prompt_tokens  3413   ← capped
sent 802 msgs -> upstream prompt_tokens  3413
```

At an 81,920-token proxy (trigger 57,344, target 24,576):

```
sent 302 msgs -> upstream prompt_tokens 19411
sent 602 msgs -> upstream prompt_tokens 39211
sent 902 msgs -> upstream prompt_tokens  3412   ← rewrite fires
sent 1202 msgs -> upstream prompt_tokens  3414   ← capped
```

Two things are wrong here, and they are different bugs:

1. **It settles at 3,412 rather than near the target.** At 81,920 the target is 24,576, so a
   conforming plan would leave roughly that much context. It leaves **14% of the target**.
   Most of the model's window goes unused, which is the opposite of the project's purpose.

2. **The result is identical at both windows.** 3,412 and 3,414 at 32k, 3,412 and 3,414 at
   81k. A plan that aims at a fraction of the window should scale with the window. This
   strongly suggests the post-plan size is being decided by something window-independent —
   most likely the fixed `keep_recent_tokens: 4096` plus the prefix, with the target playing
   no effective part.

The second observation is the more useful one, because it is a single number that should vary
and does not.

### Where to look

`plan()` computes `needed = total.saturating_sub(target)` and stops the escalation loop once
accumulated savings reach it. Two candidates:

- The loop accumulates `tokens_before - tokens_after` per step but the *terminal* step for a
  `Referenced` episode reclaims nearly the whole episode at once, so the step that crosses
  `needed` overshoots by up to a whole episode — and then the loop exits with everything
  already escalated, because each earlier step in the same pass was also accepted.
- `retained_prefix_tokens` plus `keep_recent_tokens` may be what actually bounds the result,
  in which case `target_for()` is decorative and the fix belongs there.

One measurement settles it: print `needed`, `savings` at exit, and `plan.token_after_plan()`
for a single plan and check whether `token_after_plan` tracks `target`. That is a five-minute
experiment with the fixtures in `docs/verification/`.

## What is verified

- **The deadlock is fixed.** 8 of 8 contracts pass in `docs/verification/proxy-rewrite.mjs`,
  wired into `verify.mjs` so it runs every time.
- **The structure is right.** The system prompt and newest turn survive; exactly one marker
  replaces what went; the marker sits after the system prompt; the rewrite sends less.
- **Proportionality is asserted**, not assumed: `some conversation survives the rewrite`.
  The first version of that check ran a 20,571-token transcript against a 4,096-token window
  and kept 3 messages of 602 — arithmetically correct, and not what a user wants to discover
  their proxy doing. The floor catches that.
- **Ten Rust tests** cover the plan at 4,096 / 8,192 / 32,768 windows, which no existing test
  did; every previous one used 32,768.

## Reproducing

```bash
# Growth test: watch upstream prompt_tokens after each step
sakur4d --db /tmp/rw.db --backend http://host:8080 --context-window 81920 -v \
        proxy --bind 127.0.0.1:8096 --upstream http://host:8080 --session rw &
node docs/verification/grow-session.mjs --proxy http://127.0.0.1:8096

# Contract test
node docs/verification/proxy-rewrite.mjs --bin ~/.cargo/bin/sakur4d --window 32768
```
