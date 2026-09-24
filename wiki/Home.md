# Sakur4

> **Your agent's compaction is the most expensive thing it does. Sakur4 makes it cheap, and makes it not lose anything.**

Sakur4 is a local memory and context layer for long coding-agent sessions. It runs as a single Rust
binary on your machine, speaks **MCP over stdio or HTTP**, and slots into whatever harness you already
use — no cloud, no account, no telemetry.

---

## Why this exists

An agent that works for an hour does not run out of ability. It runs out of *context*, and the way it
runs out is expensive:

| What happens today | What it costs |
|---|---|
| The harness summarises the transcript | One inference call over the whole window — and the summary is lossy |
| The summary replaces the transcript | The provider's prompt cache is invalidated; the next turn re-prefills everything |
| A constraint stated forty turns ago | Silently gone, because nothing marked it as load-bearing |
| "What did we decide about auth?" | Unanswerable — the text is not there any more |

Sakur4 addresses each of those separately rather than treating "compaction" as one problem.

## The four ideas

**1 · Cache-coherent compaction.** Eviction cuts the transcript at a boundary the inference server's KV
cache already holds, so the surviving prefix is a *prefix* of what the server has cached. The next turn
is a longest-common-prefix match instead of a full re-prefill. The engine asks the backend what it can
do and picks a strategy from the answer, rather than assuming.

**2 · Two tracks, and a model can only write to one.** A deterministic **Symbolic Ledger** records
symbols, signatures and hashes extracted by parsing — no model output, so no hallucination. A
**Semantic Atlas** holds model-written summaries, every one of which must anchor to a ledger fact, and
whose staleness is resolved *at read time* by comparing hashes. A summary that contradicts the code is
labelled `[STALE]` rather than trusted.

**3 · Graduated eviction, not a cliff.** Five tiers — `Live → Masked → Referenced → Archived → Dropped`
— applied one step at a time, so a turn that must leave the window degrades instead of vanishing. Every
plan ends in one of four reported verdicts, and `context.plan_eviction` shows the decision *before* it
is taken.

**4 · Pinned constraints survive everything.** Anchors are rendered verbatim and are exempt from all
compaction. If they ever cannot fit the budget, the operation **refuses with an error** rather than
dropping one silently.

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/sc4rfurry/Sakur4/master/install.sh | sh
```

Detects your platform, **verifies the archive checksum**, installs the binary, and places the Agent
Skill in `~/.agents/skills/`. Refuses to install an archive it cannot verify.

Then print the configuration for your harness:

```bash
sakur4d config claude     # Claude Desktop / Claude Code
sakur4d config generic-stdio
sakur4d config hermes     # the default
```

**Oh My Pi does not use `config`** — it is a native extension, installed separately:

```bash
node integrations/omp-plugin/install.mjs
```

## Where to go next

| If you want to… | Read |
|---|---|
| Get it running in five minutes | **[Getting Started](Getting-Started)** |
| Understand how eviction actually works | **[Architecture](Architecture)** |
| See every tool and its arguments | **[Tool Reference](Tool-Reference)** |
| Wire it into a specific harness | **[Harnesses](Harnesses)** |
| Know what it costs and what it saves | **[Benchmarks](Benchmarks)** |
| Run the checks yourself | **[Verification](Verification)** |
| Know what does *not* work yet | **[Limitations](Limitations)** |
| Contribute | **[Contributing](Contributing)** |

---

## Honest status

Sakur4 is **v0.1.0** and usable today. It is also a young project with a documented backlog, and the
[Limitations](Limitations) page is written to be read *before* you depend on it rather than after.

Three things worth knowing up front:

- **If you batch tool calls, await each answer.** MCP permits a server to process a queued batch in an
  arbitrary order, and Sakur4's tools are stateful. Sending a call and awaiting its reply — which every
  harness tested here does — is correct.
- **One store holds one project.** A project dimension exists for semantic memory and the transcript,
  but a session in a new directory starts with no history rather than someone else's.
- **Nothing re-indexes automatically.** `sakur4d index` after changing files; the repo map dates itself
  so you can see when it was built.

---

<sub>**Apache-2.0** · [GitHub](https://github.com/sc4rfurry/Sakur4) · [Report an issue](https://github.com/sc4rfurry/Sakur4/issues)</sub>
