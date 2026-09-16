# The proxy never rewrites, and the engine says why

Status: **open bug, reproduced, not yet fixed.**

## What happens

A transcript far past the window is forwarded **unchanged**. Reproduced with 602 messages
and 20,571 tokens measured by the real server's own tokenizer, against a proxy configured
with `--context-window 4096`:

```console
$ node docs/verification/proxy-rewrite.mjs --bin ~/.cargo/bin/sakur4d --turns 300
  transcript: 602 messages, 20571 tokens by the server's own tokenizer
  FAIL  an over-window transcript is rewritten
        expected fewer than 602 messages, saw 602
  FAIL  exactly one marker replaces what went
        expected one marker, found 0
  602 messages in, 602 out — 0 removed
```

The recorder confirms the body reached the upstream verbatim — 104,181 bytes, zero markers:

```console
$ node docs/verification/recorder.mjs --listen 8775 --upstream http://host:8080
  [recorder] 602 messages, 0 marker(s), 104181 bytes
```

## Why — the engine's own diagnostic

Asked directly, the plan reports plenty of pressure and refuses to act on it:

```console
$ # context.plan_eviction for session proxy-rewrite
pressure        : compacting
budget          : 4096
target          : 1229
live_tokens     : 27650
planned_savings : 0
updates         : 0
notes           : no episode could be escalated even though 26443 tokens of pressure exist;
                  every candidate is either at the tier floor or protected by an unresolved
                  dependency
```

That note is the same branch the `ab.mjs` benchmark hit months ago, and it is the honest
one — the planner is not failing silently. The question it raises is why 26,443 tokens of
pressure produce no escalation when the tier ladder is `live → masked → summarized →
archived` and `max_tier_step` is 1, which permits exactly one step.

## What has been ruled out

Read from the code, not assumed:

| Suspect | Finding |
|---|---|
| Candidates never collected | `score_candidates` does **not** filter on `droppable`; all 602 episodes become candidates |
| The episode is already at the floor | `propose_escalation` calls `c.current_tier.escalate()`, which returns `Some` from `Live` |
| `allow_drop` blocking it | `allow_drop: false` only gates the `Dropped` tier, and `Masked` is reached first |
| Overshoot guard | the guard only rejects when `c.tokens > still_needed * 8`; `still_needed` is ~26,421 against episodes of ~34 tokens |
| The commits never happened | `recall` returns the transcript's own text verbatim from the store |

So the refusal is inside `propose_escalation`'s final check, or in the prefix/recent window
that runs *before* it:

```rust
// Never accept an escalation that does not actually reclaim tokens.
if tokens_after >= tokens_before {
    return Ok(None);
}
```

and

```rust
let recent_start = Self::recent_start_index(&episodes, self.policy.keep_recent_tokens);
```

`keep_recent_tokens` defaults to **4096** — a fixed number, equal to this proxy's entire
budget. Under a 4096-token window the recent window and the whole budget are the same size,
so the evictable middle can be empty by construction. That is the leading hypothesis and it
is one `assert!` away from being settled.

## The likely fix

`keep_recent_tokens` is an absolute number in a policy whose other knobs became relative
(the profiles use ratios). A window-relative floor — the smaller of 4096 and some fraction
of the budget — would keep the same behaviour on a 32k or 80k window while leaving a middle
to evict on a small one.

If that is not it, the next step is to assert inside the escalation loop and read which guard
returns `None`, which is a five-minute experiment now that the reproduction is one command.

## Why this was not caught earlier

`docs/bench/ab.mjs` runs at a **32k+** window, where 4096 is a sane recent window and the
middle is large. `verify_engine.py` runs at 8192. Every existing test used a window big
enough to hide this, and the proxy's own Rust tests use a 2048 window but drive *fixtures*
whose `planned_savings` is asserted indirectly through the upstream recording — which passed
because those tests never required a rewrite to happen, only that a passthrough did not break.

The general lesson, again: the bug lives at a configuration nothing else used.

## Reproducing in one command

```bash
node docs/verification/recorder.mjs --listen 8775 --upstream http://host:8080 &
sakur4d --db /tmp/rw.db --backend http://host:8080 --context-window 4096 -v \
        proxy --bind 127.0.0.1:8091 --upstream http://127.0.0.1:8775 --session rw &
node docs/verification/proxy-rewrite.mjs --proxy http://127.0.0.1:8091 \
        --recorder http://127.0.0.1:8775 --upstream http://host:8080 --turns 300
```

`--bin <sakur4d>` makes the script own the proxy too, so it is genuinely one command.
