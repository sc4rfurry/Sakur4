# The reverse proxy rewrite path: fixed

Status: **closed.** Three defects, all found by measurement, all fixed and covered.

## Fixed: the tier ladder deadlocked

**Symptom.** A transcript far past the window was forwarded unchanged.

**Cause.** `propose_escalation` refused any step where `tokens_after >= tokens_before`. The
first rung, `Masked`, renders a header plus a 160-character preview — which for a **short
message is longer than the message**. So every candidate was refused at the first rung, none
ever reached `Referenced`, and the engine reported the same sentence every turn, forever.

**Fix.** A token-neutral step is accepted when a later rung will reclaim; only a *terminal*
step that reclaims nothing is refused. The ladder is finite, so this cannot loop.

The diagnostic was arguably worth more than the fix. The old note named two possibilities
without saying which, or how many of each; a counted breakdown produced the answer in one line:
`504 would not reclaim anything at this tier`.

## Fixed: the proxy discarded the plan whenever it reclaimed nothing

**Cause.** `rewrite_request` returned early on `planned_savings == 0`. That held only for a
session growing a turn at a time, where a later plan picks up the ladder. For a **single large
request** it was fatal: the plan advanced 903 episodes, reported zero savings, and the early
return discarded the result — 1,002 messages and 34,571 tokens forwarded verbatim against a
22,938-token trigger.

**Fix.** A message whose episode has left `live` does not belong in the window, whatever the
plan thinks it saved. Whether this turn's steps reclaimed anything is the planner's business;
whether the transcript still contains evicted content is the proxy's.

## Fixed: the same mistake, repeated in the proxy's own guard

**Cause.** `drop_evicted` required `row.render().len() < row.content.len()` — a message only
left if its replacement was literally shorter. That is exactly the reasoning that deadlocked
the planner, reintroduced one layer up. `Masked` is longer for a short message, so `doomed` was
**always empty** and the proxy forwarded everything while logging `advanced=903`.

**Fix.** Tier membership decides. `Referenced` and below represent content deliberately no
longer in the window, and a step being token-neutral is a feature of the ladder, not a reason
to ignore it. Measuring the replacement's size is the planner's job, and it does it with
`live_tokens`.

## Fixed: the planner measured a prompt that was never sent

`parts.timeline` was filled from the fabric's *rendered session timeline*, reasoning that
`assemble_parts` in `tools.rs` does that. Right for the MCP path — there the rendering **is**
the prompt — and wrong for the proxy, which sends the harness's raw `messages`. The two differ
by a factor of two, from per-episode chrome: 602 messages measured 43,712 tokens through the
timeline and 20,571 through the server's tokenizer. The timeline is now the transcript as sent.

## Fixed: `keep_recent_tokens` was absolute

4096, used directly, so the same recent context was protected whatever the window. Over a
4,096-token window that reserves the entire budget and leaves no middle to evict.
`recent_budget_for(window)` treats it as an upper bound — at most a quarter of the window,
with a 512-token floor. Behaviour at 32k and above is unchanged.

## Verified

Retained context now tracks the plan's target and scales with the window:

| window | target (30%) | settled | share |
|---|---|---|---|
| 32,768 | 9,830 | 11,461 | 117% |
| 81,920 | 24,576 | 23,408 | 95% |

A single over-window request is trimmed on the spot: 1,002 messages and 34,571 tokens in,
**335 messages and 13,397 tokens** out, with `retained=9857` against a target of `9830`.

- **10 contracts pass**, run twice with identical results.
- **The rewrite keeps a proportionate share**, and **retained tokens are compared against
  `window × 0.3`** — computed, not hard-coded. Both were added because every earlier check
  passed while the proxy kept 3 of 602 messages: "was it rewritten" and "does it send less" are
  both satisfied by erasing the conversation.
- **Repeated runs are reproducible.** The script reused one fixed store path, so run two saw
  twice the history and failed for a reason that was the test's fault.

## What made this take five rounds

Every wrong turn came from reasoning about what the code *should* do; every right one came from
measuring it. The specific traps, worth remembering:

- **A test that passes for the wrong reason.** The contract suite was green for a while only
  because a contaminated store pushed the measured size over the trigger. Its own transcript was
  under the trigger all along — 20,571 tokens against 22,938 — so it was measuring leftover
  state, not the code.
- **A stale binary.** A `spawn EPERM` invocation left a proxy running, and subsequent readings
  came from it. "Kill everything, verify nothing is listening, then measure" is not paranoia.
- **A reading believed over a reproduction.** One run showed the fix working, three later runs
  showed it not, and the discrepancy was recorded as "unreproduced" rather than resolved. It was
  real: the code did work in that path, and the failures were a *different* defect two layers
  down. Recording the inconsistency instead of explaining it away is what kept it findable.
- **The same bug twice.** The planner's `tokens_after >= tokens_before` guard and the proxy's
  `rendered.len() < content.len()` guard are the same mistake in different clothes.

## Reproducing

```bash
# Growth test: retained context against the window
sakur4d --db /tmp/rw.db --backend http://host:8080 --context-window 32768 -v \
        proxy --bind 127.0.0.1:8096 --upstream http://host:8080 --session rw &
node docs/verification/grow-session.mjs --proxy http://127.0.0.1:8096

# Contracts — run twice; the result must be identical
node docs/verification/proxy-rewrite.mjs --bin ~/.cargo/bin/sakur4d --window 32768
node docs/verification/proxy-rewrite.mjs --bin ~/.cargo/bin/sakur4d --window 32768
```
