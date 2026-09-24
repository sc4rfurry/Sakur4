# Limitations

> **Read this before you depend on Sakur4, not after.**

Every project has these. Most bury them. This page collects them in one place, states which are
*defects* and which are *scope*, and says what to do about each.

---

## The one that can affect correctness

### A queued batch is not executed in the order you sent it

**Status: open. Cause established, eight fixes attempted and reverted.**

JSON-RPC permits a server to process a batch *"as a set of concurrent tasks, processing them in any
order"*, and MCP's stdio transport correlates responses only by `id`. Sakur4's tools are stateful — the
preamble tells the model to commit a turn and then consult what it remembers — so order matters here in
a way the protocol does not guarantee.

Measured: the dispatcher serves queued requests in an **arbitrary** order, a different permutation each
run, neither first-in-first-out nor last-in-first-out.

**What you will see** if you write several frames at once and close stdin:

```jsonc
// sent as one batch
{"id":2, "method":"tools/call", "params":{"name":"memory.commit_episode", ...}}
{"id":3, "method":"tools/call", "params":{"name":"sakur4.status"}}
// the commit answers with a real episode id, and the status reports the count
// from before it — 0 for a store that now holds 1
```

**What to do:** send a call and await its answer. That is what every harness tested here does, and it is
correct. If you are writing harness integration code, do not pipeline dependent calls.

The full elimination — including the eight reverts and why each failed — is in
[`docs/DESIGN.md`](https://github.com/sc4rfurry/Sakur4/blob/master/docs/DESIGN.md).

---

## Scope, not defects

### One store keeps one project's transcript, and starts empty in a new one

Episodes carry a `project_id` and recall is scoped to the project the daemon was started for, so a
session in a new directory sees **nothing** rather than another project's work. That is deliberate. What
it is not is *shared* memory across a monorepo's packages — each project root is its own memory.

A store holding several projects reports `store_holds_other_projects` in `sakur4.status` so a count that
looks low is explained.

### Nothing re-indexes automatically

There is no filesystem watcher. Run `sakur4d index` after changing files. The repository map **dates
itself** in its output, and `sakur4://repo-map/{project}` says when it was built, so you can tell a map
from a minute ago from one from last week. `code.impact_of_change` reasons over the same stored facts.

### `sqlite-vec` is optional

Where the extension is present the vector index is in-database; where it is not, Sakur4 falls back to an
exact scan. The MCP contract is identical either way and the choice is reported in `sakur4.status`.

### One embedding model is built in

With no embedding endpoint configured, Sakur4 uses a deterministic hashing embedder — **lexical, not
semantic**. It is dependency-free and reproducible, and it will miss paraphrase. Configure a local
embedding endpoint for real semantic recall; `sakur4.status` names the embedder in use.

---

## Not yet true

### No MCP conformance run against a reference client

Both transports are exercised against a real SDK client, which is a different and weaker claim than a
conformance suite.

### No standardised long-conversation benchmark

No LoCoMo, no Endurance Benchmark. The A/B in [`docs/bench/`](https://github.com/sc4rfurry/Sakur4/blob/master/docs/bench/README.md)
is a real measurement but a narrow one: matched windows, one repository, one model. It does not
substitute for a standardised benchmark, and the [Benchmarks](Benchmarks) page says which numbers came
from where.

### No live agent session through Hermes

The Hermes engine passes 44 contracts against a live daemon, and its transport is verified, but driving
Hermes with a real model has not succeeded — its provider routing rejects the model string this setup
needs. The engine is tested; the end-to-end session is not.

### The OMP compaction hook has never fired for real

The tool path is verified end to end by a live model. Forcing OMP past its context limit, so that its own
compaction path runs against Sakur4, is separate work.

### Release artifacts are checksummed, not signed

The release publishes archives with `SHA256SUMS.txt`, and `install.sh` **verifies the checksum and
refuses to install without it**. What is missing is a signature: a checksum proves a download was not
corrupted, not that it came from this project.

### Three `PromptParts` slots are populated only by tests

`PromptParts` renders eight parts; every production caller uses five. `with_repo_map`,
`with_tool_schemas` and `with_folds` are called only from `prompt.rs`'s own tests and the testkit — so
the *assembler* can render them, but no live request does.

### Nothing ever marks an episode superseded

The eviction engine's scoring reads `superseded_by`, and no code path writes it. The column exists and is
always null.

---

## Process gaps

| Gap | Why it matters |
|---|---|
| `cargo deny` is not run | Covers licences and duplicate versions — a policy question this project has not answered, not a defect |
| FR-19's acceptance test is unrun | The suite has not been executed with network egress blocked at the OS level |
| `cargo audit` runs in CI, but only since recently | Running it by hand the first time found a medium-severity `rustls` advisory published ten days earlier |

---

<sub>[← Back to Home](Home) · [All pages](Home#where-to-go-next)</sub>
