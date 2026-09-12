<div align="center">

<img src="docs/assets/hero.svg" alt="Sakur4 — cache-coherent memory and context for local coding agents. Three prompt layouts compared: a summarising compaction rewrites the prefix so the server's cache matches nothing and the whole context is re-prefilled; Sakur4 evicts after a checkpoint the server can rewind to, so 4034 tokens stay byte-identical and 24% of the prefill is avoided." width="100%">

<br>

**Your agent's compaction is the most expensive thing it does.**

Sakur4 is an MCP server, a native Oh My Pi extension, and a portable Agent Skill that
give a coding agent persistent memory — and make its context compaction cheap instead
of ruinous.

<br>

[![CI](https://github.com/sakur4/sakur4/actions/workflows/ci.yml/badge.svg)](https://github.com/sakur4/sakur4/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/sakur4/sakur4?color=22d3ee&label=release)](https://github.com/sakur4/sakur4/releases)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.94%2B-orange.svg)](https://www.rust-lang.org)
[![MCP](https://img.shields.io/badge/MCP-2026--07--28-8b5cf6.svg)](https://modelcontextprotocol.io)
[![Tests](https://img.shields.io/badge/tests-223%20passing-34d399.svg)](#verification)

[Quick start](#quick-start) ·
[Connect a harness](#connect-your-harness) ·
[How it works](#how-it-works) ·
[Tools](#the-tool-surface) ·
[Verification](#verification) ·
[Limitations](#limitations)

</div>

---

## The problem

An agent harness compacts when the context window fills. It replaces the transcript
with a summary and sends the result. That new token sequence shares no prefix with the
old one, so llama.cpp's longest-common-prefix slot matching finds nothing and the
**entire** compacted context is re-prefilled — 100+ seconds for a 50K-token session on
consumer hardware.

The operation whose purpose was to make the session cheap becomes the most expensive
thing in it.

Nothing in that loop is wrong. The harness and the inference server simply do not know
about each other. **Sakur4 knows about both.**

<img src="docs/assets/coherence.svg" alt="Evicting before consulting the cache produces a boundary at token zero, which no checkpoint can align to. Asking the cache first and evicting after it preserves a reusable prefix. Every plan ends in one of four reported verdicts, and the fallback path always produces a correct plan." width="100%">

The order of operations *is* the design. Three versions of that logic were written
before one was right, and each wrong version left the entire test suite green — which
is why the claim is now stated as executable contracts in
[`cache_coherence.rs`](crates/sakur4-core/tests/cache_coherence.rs) rather than as a
promise in a README.

---

## Quick start

No GPU, no model, no network. The embedded backend simulates a llama.cpp checkpoint
ring in-process, so the whole system is exercisable anywhere.

```bash
cargo install sakur4d

# A guided walkthrough: dual-track write, staleness detection, a real compaction,
# the cache verdict, round-trip integrity.
sakur4d demo --db :memory:
```

> **Until v0.1.0 reaches crates.io**, install from a
> [release binary](https://github.com/sakur4/sakur4/releases) or build from a checkout
> with `cargo build --release`. The release archives carry `sakur4d`, the skill and the
> OMP plugin together, so a download is a complete install.

<details>
<summary><b>See what it prints</b></summary>

```text
=== 5 · the eviction decision ===
  pressure       Compacting
  budget         32768 · trigger 24576 · target 18022
  live 24847 · anchors 69 · fixed 73
  6 episode(s), 7812 tokens reclaimed (24989 → 17177 of 18022 target).
    partial reuse — prefix survived compaction

=== 6 · applying it, and what the cache did ===
  cache: partial reuse — prefix survived compaction
    window 17114/32768 tokens (52% full)
    cache: partial-reuse
      4034 tokens reused from the LCP, 13080 prefilled (24% saved)

=== 7 · round-trip integrity (FR-5) ===
  recalled 3 evicted episode(s) verbatim — content is unchanged by eviction
```

</details>

That receipt is the same one `context.receipt` returns over MCP, and that plan is the
same one `context.plan_eviction` returns. Nothing in the demo is a special path.

---

## Connect your harness

Three routes, because harnesses disagree about what they support. All of them reach the
same daemon and the same memory.

### 1 · MCP — any client

```bash
sakur4d config hermes          # ~/.hermes/config.yaml
sakur4d config claude          # claude_desktop_config.json
sakur4d config claude-code     # one-line CLI registration
sakur4d config generic-http    # anything that connects to a URL
sakur4d config generic-stdio   # anything that spawns a child process
```

`config` prints ready-to-paste configuration with this binary's absolute path and store
baked in, so there is no placeholder to forget.

```bash
sakur4d serve                             # stdio (default)
sakur4d serve --transport http --bind 127.0.0.1:8765
```

Verified against a real install:

```console
$ hermes mcp test sakur4
  Testing 'sakur4'...
  Transport: stdio → D:\DuDu\Sakur4\target\debug\sakur4d.exe
  ✓ Connected (5765ms)
  ✓ Tools discovered: 17
```

### 2 · Oh My Pi — native extension

OMP has **no MCP client**, so it needs a native TypeScript extension. That turns out to
be an advantage: an extension can see inside the agent loop, so Sakur4 gets hooks a tool
provider cannot reach.

```bash
node integrations/omp-plugin/install.mjs
```

> The installer exists because `omp install` symlinks, which fails on Windows with a
> bare `EPERM` unless Developer Mode is on. It also writes the lockfile entry that OMP's
> loader otherwise skips in silence — the difference between a plugin that *looks*
> installed and one that loads.

| Hook | What it does |
|---|---|
| `before_agent_start` | injects the working preamble once per session |
| `context` | retrieves memory for the prompt, capped, **reporting its own token cost** |
| `message_end` | forwards provider token usage every turn, automatically |
| `session_before_compact` | replaces blind summarisation with Sakur4's planned eviction |
| `session_shutdown` | reports stale summaries, because the next session inherits them |

See [`integrations/omp-plugin/`](integrations/omp-plugin/).

### 3 · Agent Skill — no MCP, no extension

`skills/sakur4/` is a portable [Agent Skills](https://agentskills.io/specification)
package: a `SKILL.md` plus a **dependency-free** Node CLI over the daemon.

```bash
node integrations/omp-plugin/install.mjs --skill-only   # → ~/.agents/skills/
```

`~/.agents/skills/` is the standard location, so OMP, Claude Code, Codex and pi all
pick it up with no further configuration. Progressive disclosure means only the
description sits in context until a task matches.

---

## How it works

<img src="docs/assets/architecture.svg" alt="Two harnesses reach Sakur4 over MCP; two have no MCP client and reach the daemon directly. The daemon holds eight components over a SQLite store, and probes its inference backend rather than assuming its capabilities." width="100%">

### Memory is two tracks, and no model can write to the first

<img src="docs/assets/dual-track.svg" alt="A tool result is stored verbatim, then split: deterministic parsers write facts to the Symbolic Ledger, which no model can write to, while model interpretation goes to the Semantic Atlas with a mandatory anchor. Staleness is computed at read time by comparing the anchor's stored hash with its current one." width="100%">

The guarantee is structural, not aspirational:

| Guarantee | Enforced by |
|---|---|
| A model cannot write the Symbolic Ledger | `SymbolicFact` has one constructor, and it demands a `FactSource`. The module imports nothing that could reach an inference client. |
| Recorded content cannot be altered | `UPDATE` and `DELETE` on episode content are blocked by database triggers, so "an evicted episode recalls byte-identically" holds for code not yet written. |
| Anchors cannot be evicted | Eviction selects from episodes; anchors live in a different table. The operation is not expressible. |
| Library code does not panic | Zero `unwrap`/`expect`/`panic!` paths outside tests. Malformed harness input is a typed error. |

### Every plan ends in one of four reported verdicts

`aligned` · `snapped` · `partial-reuse` · `full-re-prefill`

**The fallback is first class.** With no server, an older build without `/slots`, or a
sliding-window model whose checkpoints carry only partial state, the plan is still
produced, still evicts, and says why alignment was impossible. Sakur4 is always correct;
it is only sometimes not optimally fast.

### A turn, end to end

<img src="docs/assets/turn-lifecycle.svg" alt="Nine steps across a session: probe, assemble and plan the boundary, retrieve, call the model, report usage, commit, propose pins, compact at the trigger, and flush. Each is optional, and with no daemon the harness behaves as if Sakur4 were absent." width="100%">

### Cache accounting, local and cloud

A local llama.cpp slot reports its cache state through its own API. A hosted provider has
no such API — but it does report, in every response, how many prompt tokens came from its
prompt cache.

```jsonc
// The harness reports what the provider said.
context.record_usage { "session_id": "s", "prompt_tokens": 6200,
                       "cache_read_tokens": 300 }

// Sakur4 answers with a verdict.
{ "verdict": "PREFIX-BROKEN", "regression": true,
  "detail": "the provider's cached prefix fell from 5000 to 300 tokens (4700 tokens
             no longer cached) while the prompt went 6000 → 6200; this turn was
             billed for history that had already been paid for" }
```

The signature is an inversion: append-only growth makes the cached prefix *grow*, while a
rewrite that replaces a long prefix with a shorter one makes it *shrink* even as the
prompt stays large. Hermes' own documentation calls that invalidation "the strongest
argument against" per-turn compaction and notes the trade depends on numbers specific to
the user. **Sakur4 supplies those numbers.**

Sakur4 never calls a provider itself. It is a subsystem, not a harness.

---

## The tool surface

17 tools, 4 resources, 1 prompt, targeting MCP revision **2026-07-28** with
`ttlMs`/`cacheScope` on list responses.

| Tool | What it does |
|---|---|
| `memory.commit_episode` | Append a turn or tool result. Structured output becomes deterministic facts; the text stays verbatim. |
| `memory.pin` | Pin a constraint into the Anchor Set. Exempt from every eviction tier. |
| `memory.recall` | Hybrid search. Stale summaries return with their source's **current value**. |
| `memory.fold` / `memory.unfold` | Open and close an isolated sub-context. The trace leaves the window; the result stays. |
| `memory.recall_fold` | Retrieve a folded subtask's episodes, verbatim and in order. |
| `memory.staleness` | Which stored interpretations no longer match their source, and why. |
| `code.get_repo_map` | A token-budgeted structural outline, ranked by how load-bearing each symbol is. |
| `code.query_symbol` | A symbol's **current** signature from the parser. Cannot be stale. |
| `code.impact_of_change` | Every call site that depends on a symbol, transitively. |
| `session.snapshot` / `session.restore` | Persist and reload a slot's KV state. |
| `context.receipt` | Where the token budget went, and the cache verdict. |
| `context.plan_eviction` | What the engine would evict right now, and why. Plans by default. |
| `context.record_usage` | Report provider token usage; get a prompt-cache verdict. |
| `sakur4.status` | Resolved backend, detected cache capabilities, store counts. |
| `sakur4.dream` | One memory-maintenance pass: promote, regenerate, embed, archive. |

**Resources** — `sakur4://repo-map/{project}`, `sakur4://receipt/latest`,
`sakur4://anchors/{project}`, `sakur4://status/{project}`.

**Prompt** — `sakur4_system_preamble`, naming *when* to call each tool rather than what
it does, because the failure mode with smaller instruction-tuned models is
under-triggering: they have the tools and do not reach for them.

---

## Verification

<img src="docs/assets/verification.svg" alt="Nine verified behaviours including the test suite, both MCP transports, and live harness discovery, against nine unverified items including a real llama.cpp server, a live Hermes session, and the OMP compaction hook." width="100%">

A release claim is worth exactly as much as the evidence behind it, so here is the
evidence.

**What the tests actually do.** They are not all unit tests. `stdio_transport.rs` spawns
the real binary and speaks JSON-RPC over its pipes; `gateway.rs` drives the tool surface
over a live HTTP listener using the SDK's own client; `cache_coherence.rs` states the
central claim as contracts and fails loudly if the preserved prefix stops being a byte
prefix of what the server is sent.

That test suite found two bugs no amount of self-testing would have: one unconstrained
output schema made Hermes reject the **entire** tool catalog, and a WARN-level log line
written to stdout corrupted the JSON-RPC channel.

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features   # RUSTFLAGS=-D warnings
cargo test --workspace --all-targets
cargo doc --workspace --no-deps                          # RUSTDOCFLAGS=-D warnings
```

CI runs all four on Linux, macOS and Windows, plus a release-profile build, an MSRV
build at the declared 1.94, and a `cargo publish` dry run.

---

## Repository layout

<img src="docs/assets/repo-map.svg" alt="Three Rust crates — the engine, the daemon and a test kit — plus harness integrations and a portable skill package." width="100%">

```
crates/sakur4-core/       the engine — no transport, no MCP
crates/sakur4d/           the daemon — CLI + MCP gateway
crates/sakur4-testkit/    fixture repos, a fake llama.cpp server
integrations/omp-plugin/  native Oh My Pi extension + installer
skills/sakur4/            portable Agent Skills package
docs/DESIGN.md            how each requirement is met, and the trade-offs taken
docs/RELEASING.md         how to cut a release, and what is manual and why
docs/assets/              the figures above, and the generator that draws them
```

The figures are generated by [`docs/assets/generate.mjs`](docs/assets/generate.mjs) —
they quote real measurements from `sakur4d demo`, so when the engine changes they are
regenerated rather than left to drift.

---

## Configuration

Everything is optional; the defaults work.

| Variable | Default | Meaning |
|---|---|---|
| `SAKUR4_DB` | `sakur4.db` (daemon) · `~/.sakur4/sakur4.db` (skill) | Memory store |
| `SAKUR4_BIN` | searched | Path to `sakur4d` |
| `SAKUR4_SESSION` | derived | Session id |
| `SAKUR4_BACKEND` | `auto` | `auto` · `embedded` · `none` · a llama.cpp base URL |
| `SAKUR4_EMBED_API_KEY` | unset | Key for a configured embedding endpoint |

```bash
# Against a real llama.cpp server
llama-server -m model.gguf -c 65536 --slots -cms 256 -ctxcp 64
sakur4d --backend http://127.0.0.1:8080 doctor
```

`doctor` prints exactly which cache endpoints were detected and what that means for
compaction. `--backend` accepts a URL on another machine.

---

## Limitations

Stated plainly, because the alternative is finding out later.

- **No real llama.cpp server has been contacted.** The adapter is tested against a fake
  server speaking the documented routes over real HTTP, so the *client* is exercised —
  but first contact with a real build is still first contact.
- **No live agent session through Hermes.** Its transport is verified and the OMP tools
  were driven end-to-end by a live model, but Hermes' own tool selection is untested.
- **The OMP compaction hook has never fired for real.** The tool path is verified;
  forcing OMP past its context limit is separate work.
- **OMP 18.1.17 is the tested version.** The extension API is undocumented and was
  reverse-engineered from the shipped type definitions.
- **Encryption at rest is not implemented** (FR-20). The store contains a verbatim
  transcript; treat it as exactly as sensitive as the sessions it recorded.
- **No authentication on the transports.** Localhost binding is the control. `--bind
  0.0.0.0` exposes the whole Memory Fabric to anyone who can reach the port.
- **No MCP conformance run, no LoCoMo, no Endurance Benchmark.** NFR latency and memory
  numbers are unmeasured on reference hardware.
- **`cargo deny` / `cargo audit` are not in CI.** Review `Cargo.lock` changes.

The full list, including every deviation from the source requirements, is in
[docs/DESIGN.md](docs/DESIGN.md).

---

## Contributing

Contributions are welcome. [CONTRIBUTING.md](CONTRIBUTING.md) covers the invariants worth
knowing before changing anything — the four above, plus why the cache-coherence code is
the most delicate part of the project and why a change there should make you suspicious
of a green test suite.

Security problems: please report privately per [SECURITY.md](SECURITY.md).

---

## License

[Apache-2.0](LICENSE).

<div align="center">
<br>
<sub>Named for the <i>sakura</i> — and for the <code>4</code> in <code>sakur4d</code>, which is what you get when the name you want is already taken.</sub>
</div>
