# Sakur4 for Hermes — the ContextEngine plugin

Implements **FR-16**. Replaces Hermes' summarising context engine with Sakur4's
Graduated Eviction Engine, and closes a loop the MCP-only integration could not.

## What changes

Hermes normally compacts by asking a model to summarise the transcript. That works, and
it invalidates the prompt cache — the rewritten prompt shares no prefix with what the
provider already holds. Hermes' own documentation calls this *"the strongest argument
against"* per-turn compaction.

With this engine, compaction is decided by token counts, recency, graph in-degree and
explicit droppability, and it preserves a prefix while doing it.

| Hermes hook | What this engine does with it |
|---|---|
| `update_from_response(usage)` | Forwards the provider's token accounting — including cache read/write counts — to Sakur4 on **every** call. This is the loop MCP alone could not close: prompt-cache behaviour is now measured automatically rather than reported by hand. |
| `should_compress` | Fires at the same threshold Hermes would, so behaviour stays predictable. |
| `compress` | Commits every message to the append-only stream, asks Sakur4 what to evict, and replaces exactly those messages with a marker. |
| `select_context` | Injects the Anchor Set into every request, after the system prompt. |
| `get_tool_schemas` / `handle_tool_call` | Exposes `sakur4_recall`, so the model can look something up rather than reconstruct it. |
| `prune_tool_results_only` | Commits but prunes nothing — see below. |
| `__deepcopy__` | Copies budget state and builds a fresh client, because Hermes deep-copies the engine for sub-agents. |

## Install

```bash
cp -r integrations/hermes-plugin "$LOCALAPPDATA/hermes/plugins/sakur4"

# Start the daemon the engine talks to.
sakur4d serve --transport http --bind 127.0.0.1:8770 --context-window <your model's window>

# Select the engine in ~/.hermes/config.yaml
#   context:
#     engine: sakur4
```

`hermes plugins list` should show `sakur4` with source `user`.

## Configuration

| Variable | Default | Meaning |
|---|---|---|
| `SAKUR4_URL` | `http://127.0.0.1:8765` | Where the daemon is listening |
| `SAKUR4_BIN` | searched | Path to `sakur4d`, used by the same search order as the OMP extension |
| `SAKUR4_DB` | daemon default | Store path, for reference |
| `SAKUR4_SESSION` | `hermes` | Session id |
| `SAKUR4_THRESHOLD_PERCENT` | `0.75` | Fraction of the window at which compaction fires |

## Verifying it

Two things are worth verifying separately, and only one of them is done.

**The engine, against a live daemon** — done. `verify_engine.py` exercises it against a
running process. It is a script rather than a unit test because the engine's whole job is
to talk to a daemon and satisfy an interface Hermes owns; mocking the daemon would test
the mock.

**A Hermes session driving a model with this engine active** — **not done.** The engine
registers and is instantiated by a real Hermes install, but a one-shot session in an
isolated profile could not be routed to a local model, and the cause is Hermes'
provider resolution rather than this engine. The configuration that *does* work, the three
enabling details that are easy to miss, and exactly where it stopped are recorded in
[LIVE-TESTING.md](LIVE-TESTING.md), so finishing it does not mean repeating that search.

```bash
# Terminal 1
sakur4d --db /tmp/hermes.db --backend embedded \
        --context-window 8192 serve --transport http --bind 127.0.0.1:8770

# Terminal 2
SAKUR4_URL=http://127.0.0.1:8770 python integrations/hermes-plugin/verify_engine.py
```

It exits non-zero on the first failed contract, so it can gate a release. 44 contracts
covering threshold arithmetic, anchor injection, compaction, recall after eviction,
provider accounting, deepcopy, and the offline path.

**Pass `--context-window`**, and make it small enough that the test's synthetic history
exceeds the trigger. Without it the daemon plans against the backend's reported window
(the embedded backend simulates 32,768), the test's 11k tokens sit below the trigger, and
the plan correctly reclaims nothing — which reads as "the engine cannot compact" when it
is the test that has not applied enough pressure.

## Three bugs this plugin had, and what they teach

All three were found by running it, none by reading it.

**1. Anchors never loaded.** The daemon requires the 2026-07-28 per-request `_meta` block
on `resources/read`, exactly as on `tools/call`. Omitting it returns
`400 Invalid params: request _meta is missing`, and the first version discarded the error
body — so the failure was indistinguishable from "there are no anchors", and every pinned
constraint stayed invisible to the model. The same bug existed in the OMP extension for
its tool calls.

**2. Compaction silently did nothing.** The plan identifies what to evict by
`episode_id`. The engine expected `to` and `excerpt` fields, matched nothing, and
returned the messages unchanged while reporting success. It now records which episode
each message became and matches on that. **This is the worst failure mode available
here**: had it shipped, the host would have believed the context shrank, and the only
symptom would have been a context window that kept overflowing.

**3. `--context-window` had no effect.** The embedded backend simulates a slot and
reports a fixed 32,768, and `context_window()` asked the backend first — so a user's
explicit setting lost to a number that describes nothing. It is now optional in the CLI,
so "the user set this" is distinguishable from "nobody said", and an explicit value wins.

## `prune_tool_results_only` deliberately does nothing

Hermes calls it on a low, cost-oriented trigger to reclaim re-sent tool output. Sakur4's
plan already treats re-runnable tool results as its first eviction candidates, so pruning
here as well would evict twice for one saving — and a tool result dropped on a cheap
trigger cannot be recovered by a later plan, because it is already gone from the window.

The commit still happens, so the content stays retrievable either way.

## Failure policy

Every daemon call is best-effort. If `sakur4d` is not running, `compress()` returns the
messages **unchanged** and Hermes handles the overflow with its own fallback. Inventing a
summary on an empty plan would silently discard a conversation, and a context engine that
breaks a session when its sidecar is down is worse than no context engine.
