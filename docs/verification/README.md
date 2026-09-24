# Verification against a real llama.cpp server

Everything in this directory talks to a **real** server over HTTP. Nothing here is a
mock. This is the record of what was measured, what it establishes, and — as
importantly — what it does not.

## The server under test

| | |
|---|---|
| Endpoint | `http://your-llama-server:8080` (remote, reached over a private network) |
| Model | a 27B model at Q3_K_XL |
| Build | the llama.cpp build current at the time |
| Context | 81,920 tokens |
| Slots | 1 |
| Platform | Windows |

## Headline results

### The capability probe — Sakur4's diagnosis was correct

| Route | Status | Meaning |
|---|---|---|
| `/slots` | 200 | slot listing works, but the payload is thin: only `id`, `n_ctx`, `speculative`, `is_processing` |
| `/slots/0` | **404** | no per-slot detail route in this build |
| `/slots/0?action=save` | **501** | slot save is not implemented |
| `/slots/0?action=erase` | **501** | slot erase is not implemented |
| `/slots/0/checkpoints` | **404** | no checkpoint ring |
| `/tokenize` | 200 | exact tokenisation available |
| `/props` | 200 | advertises no checkpoint configuration |

`sakur4d doctor` against this server reports:

```
capabilities     slots+tokenize
coherence        no checkpoint source detected — compaction will report full re-prefill
```

That is correct, and it is the PRD's top-rated risk — *"llama.cpp's slot/checkpoint API
is a moving target"* — occurring in the wild. The probe degraded gracefully rather than
assuming, which is exactly what NFR-7 asks for.

### Prefix reuse works anyway, and it is large

Explicit checkpoints are not the only mechanism. llama.cpp caches the longest common
prefix of an incoming prompt automatically. Measured on this server with a 3,045-token
prompt, `n_predict = 1`, `cache_prompt = true`:

| Case | prompt processed | cache reused | wall clock |
|---|---|---|---|
| A — cold | 3,045 | 0 | **2,743 ms** |
| B — identical prompt again | 4 | 3,041 | **162 ms** |
| C — same prompt, head changed | 3,060 | 0 | **2,426 ms** |

Reproduced on a second run at a smaller prompt size (2,017 tokens): cold 1,853 ms,
identical resend 159 ms, head changed 1,704 ms — the same shape at 0.92 ms/token.
The numbers are stable across runs, not a single lucky measurement.

- **Prefill costs 0.89 ms/token** — about **1,118 tokens/second** on a 27B at Q3.
- **Re-sending an identical prompt is 94% faster**, and the server reports only 4 tokens
  processed.
- **Changing the head costs 15–17×** what the cached version costs. That is the failure
  Sakur4 exists to prevent, reproduced on real hardware rather than in a simulation.

### The case Sakur4 actually produces

Reuse requires *new material after the shared prefix*. Holding the divergence position
fixed and growing the tail (`reuse-rule.mjs`, section A):

| New tokens after the cut | prompt tokens | reused | processed | reused % |
|---|---|---|---|---|
| 0 | 2,219 | 0 | 2,219 | 0% |
| 1 | 2,221 | 2,219 | 2 | **100%** |
| 2 | 2,227 | 2,221 | 6 | **100%** |
| 8 | 2,263 | 2,239 | 24 | 99% |
| 32 | 2,405 | 2,263 | 142 | 94% |
| 512 | 2,785 | 2,405 | 380 | 86% |

A 2,219-token preserved prefix followed by new content is reused **in full**. That is
precisely the shape Sakur4's eviction produces, so its core mechanism is sound on this
hardware.

A prompt that is a *strict prefix* of what is resident — nothing new after the cut —
reuses nothing, because there is no next-token position to evaluate. That is coherent
rather than surprising, and it means a compaction must leave a non-empty tail.

## What this changes about Sakur4

**The receipt is pessimistic on this backend.** `sakur4d demo --backend
http://your-llama-server:8080` reports:

```
cache: FULL RE-PREFILL — compaction broke the prefix
no checkpoints are available on this backend/session; full re-prefill
0 tokens reused / 44705 prefilled (0% saved)
```

The *diagnosis* is right — there is no checkpoint API to align to. The *prediction* is
wrong: llama.cpp will reuse a preserved prefix without any checkpoint, so the actual
saving is not zero. At the measured 0.89 ms/token, preserving a 4,000-token prefix is
worth roughly **3.5 seconds** per compaction; preserving 20,000 tokens is worth about
**18 seconds**.

Two distinct claims are being conflated in that message, and they should be separated:

1. *"I cannot choose a cache-aligned boundary"* — true here, and unavoidable.
2. *"therefore you will pay a full re-prefill"* — not true here, and it is the number a
   user would act on.

The honest fix is a third verdict alongside the existing four: **`prefix-preserved,
alignment unknown`** — we kept a prefix, the backend exposes no checkpoint to confirm it
against, and LCP-based reuse is expected but unverified. That is what the evidence
supports and no more.

## Non-functional requirements, measured

The PRD's own performance targets, run on the development machine rather than the
PRD's reference hardware (which this box is not).

| NFR | Target | Measured | Verdict |
|---|---|---|---|
| **NFR-1** incremental re-index, 10,000 files | < 2 s | **1.55 s** (full index 14.5 s, store 78.6 MB) | pass, **tight** |
| **NFR-2** `memory.recall` at 100,000 entries | < 300 ms | **88 ms** worst p95 | pass |
| **NFR-3** boundary decision overhead | < 50 ms | **71 ms** including process start and store open | **inconclusive** |
| **NFR-4** idle RSS | < 200 MB | **7.7 MB** | pass |
| **NFR-5/6** transactional writes, crash-safe resume | no corruption | `a_restart_preserves_the_store_and_resumes` | pass |
| **NFR-7** graceful degradation with no checkpoint API | never a hard failure | confirmed against the real server above | pass |

NFR-2 at full scale, by query class. The distribution matters more than the mean: a
term present in every episode exercises the ranker, while a unique token exercises only
the index, and an average would hide whichever is slow.

```
  query class            min     median      p95      max
  ubiquitous term          59ms        62ms       65ms       65ms
  mid-frequency term       61ms        68ms       88ms       88ms
  unique token              2ms         2ms        2ms        2ms
  unique marker             2ms         2ms        5ms        5ms
  two-term phrase           3ms         3ms       78ms       78ms
  absent term               1ms         1ms        2ms        2ms
```

**NFR-3 is not properly measured.** The 71 ms is a whole `sakur4d plan` invocation —
process spawn, store open, engine construction, and the decision. The requirement is
about the decision's overhead *inside a running session*, which needs an in-process
benchmark rather than a CLI invocation. The honest reading is that the budget is not
obviously exceeded, not that it is met.

```bash
node docs/verification/nfr2-recall.mjs --n 100000     # ~5 minutes of seeding
```

## Distribution

**Installable, and the release has been exercised end to end.**

| | State |
|---|---|
| GitHub repository | public — <https://github.com/sc4rfurry/Sakur4> |
| release | `v0.1.0`, five platforms, with `SHA256SUMS.txt` |
| install script | `README.md`'s one-liner, confirmed to fetch and verify |
| `repository` in `Cargo.toml` | the real URL, checked by `repo-url.mjs` on every run |
| crates.io | **not published** — `sakur4-core` must go first, then `sakur4d` |
| `cargo audit` / `cargo deny` | **not in CI** |
| signed artifacts | **no** — checksums detect corruption, not tampering |

The last three are the remaining distribution gaps, and none is configuration: one needs a
crates.io token, one needs a vulnerability feed wired into CI, and one needs a signing identity.

**On the publish order:** `cargo publish -p sakur4d` fails with `no matching package named
'sakur4-core' found` until `sakur4-core` is published, because the dependency is by version.
`docs/RELEASING.md` states the order, and `ci.yml` fails if the README stops documenting it. The
failure it prevents — `sakur4d` consumed before its dependency exists — cannot be undone.

**This section previously said the opposite of every line above.** It reported no remote, no
repository, an unpushed tag, and `Cargo.toml` holding "a placeholder" — while the same table named
the real URL, three lines after claiming the repository did not exist. It was written when all of
that was true and not revisited when it stopped being.


## What is *not* established

Stated so nobody mistakes this for more than it is.

- **The exact reuse rule is not fully reverse-engineered.** Some measurements show zero
  reuse in cases where prefix matching alone predicts a hit — for instance a shortened
  prompt sent immediately after a longer one. Whether that is request ordering, a
  per-request checkpoint, or a heuristic is unresolved. Everything reported above was
  reproduced, but the boundary conditions are not pinned down.
- **These are deterministic, synthetic prompts.** Real agent transcripts contain more
  repetition, which may tokenise differently and cache differently.
- **One model, one build, one machine.** `the current build` is not every build, and the server's
  `--cache-reuse` and `--ctx-checkpoints` flags were not inspected — only the HTTP
  surface was.
- **`n_predict = 1`, so generation cost is excluded** from the timings. That is
  deliberate: it isolates prefill, but it is not a whole-turn measurement.
- **The earlier scripts in this directory contain recorded failures** — a prompt
  generator that produced 156,126 tokens from a request for 3,000, and a variant sweep
  whose prompts silently overflowed the context window. Both are documented in the
  files themselves, because a measurement script that lies quietly is worse than no
  script.

## Running these

Every script here is dependency-free — plain Node or plain PowerShell — and runs standalone.
Some need the inference server, some need a built `sakur4d`, and some need neither.

### Against a real llama.cpp server

```bash
BASE=http://host:port

# Headline numbers: cold vs cached vs head-changed.
node docs/verification/llamacpp-prefix.mjs --base $BASE --tokens 3000

# Reuse against tail size and divergence position, one variable at a time.
node docs/verification/reuse-rule.mjs --base $BASE

# What lengthening and shortening a prompt do.
node docs/verification/shortening-probe.mjs --base $BASE

# Is a short run of characters a token boundary? (No.)
node docs/verification/lcp-diagnose.mjs --base $BASE
```

None of those four need Sakur4 at all. They characterise the server, which is what everything
else here depends on.

### Against a built daemon

```bash
BIN=~/.cargo/bin/sakur4d
BASE=http://host:port

# Recall latency at scale (NFR-2: target < 300 ms p95).
node docs/verification/nfr2-recall.mjs --n 100000 --bin $BIN

# A real transcript through the reverse proxy: is it trimmed, marked, proportionate?
node docs/verification/proxy-rewrite.mjs --bin $BIN --upstream $BASE --window 32768

# How much context survives as a session grows, read from the server's own token counts.
node docs/verification/grow-session.mjs --proxy http://127.0.0.1:8090

# Does every generated harness config name an absolute store path?
node docs/verification/config-paths.mjs $BIN

# Do the exact command shapes the OMP extension builds still work?
node docs/verification/omp-commands.mjs $BIN

# Does every tool name an integration calls exist in the daemon's catalog?
node docs/verification/tool-names.mjs $BIN

# Snapshot / restore (FR-8): the round trip, and the refusal on a backend without the API.
node docs/verification/snapshot-roundtrip.mjs --bin $BIN --backend embedded
node docs/verification/snapshot-roundtrip.mjs --bin $BIN --backend $BASE

# Usage accounting (FR-15): report through the tool, then read the receipt back.
node docs/verification/usage-roundtrip.mjs --bin $BIN

# Does every environment variable the docs name actually get read?
node docs/verification/env-vars.mjs

# Do the documents agree with the daemon about how many tools it exposes?
node docs/verification/catalog-counts.mjs --bin $BIN

# Does every command the docs tell a reader to run actually exist?
node docs/verification/doc-commands.mjs --bin $BIN

# The install path, against the published release: fetch, verify, refuse a mismatch, extract, place.
# Needs `--upstream` in `verify.mjs` because it reaches the network.
sh docs/verification/install-path.sh

# Does every dependency a crate declares appear in its source?
node docs/verification/unused-deps.mjs

# Does every relative link in the docs point at something that exists?
node docs/verification/doc-links.mjs

# Do the prompt-building paths still route anchors through FR-4's budget check?
node docs/verification/anchor-wiring.mjs

# Candidates for reading, not a verdict: public functions nothing here calls. Not wired into
# erify.mjs, because most of what it lists is legitimate library surface — read the doc comments.
node docs/verification/uncalled.mjs
```
### Diagnostics, for when something looks wrong

```bash
# A recording reverse proxy: forwards to the server and dumps each request as JSON.
node docs/verification/recorder.mjs --listen 8775 --upstream $BASE --dump /tmp/dump

# A listener that captures what a harness actually sends, with credentials redacted.
pwsh docs/verification/capture-request.ps1 8772

# What the server reuses, when longest-common-prefix alone does not explain it.
node docs/verification/reuse-probe.mjs --base $BASE

# What is in a store, by session, and what anchors each atlas entry.
node docs/verification/store-inventory.mjs --db ~/.sakur4/sakur4.db
node docs/verification/atlas-inventory.mjs --match "some text"

# Move named sessions out of recall's window. Backs up first, has no --all, and reports what
# actually changed rather than what it attempted.
node docs/verification/store-purge.mjs --db PATH --archive --dry-run --session NAME

# Is the repository URL real everywhere it appears?
node docs/verification/repo-url.mjs

# Do the README's documented tool arguments actually exist on the tools?
node docs/verification/tool-args.mjs

# Do the workflows' shell blocks parse? Two shipped with syntax errors only CI could see.
node docs/verification/workflow-shell.mjs
```

**`episodic_stream` is append-only by trigger**, so episodes cannot be deleted — that is FR-1 and
it is the point. A session can be *archived* out of recall's window instead, because the trigger
covers `content, role, tool_name, seq, session_id, episode_id` and not `eviction_tier`.

Archiving an episode is not sufficient on its own. Recall also returns `semantic_entry` results
from the atlas, and those are filtered by **their own anchor**, not by the episode's tier — so a
session whose episodes are archived can still surface through its summaries. `store-purge --archive`
does both.

These exist because a verification script had been writing its fixtures into the *default* store
rather than a temporary one. The scripts are fixed; the tools are what cleaned up after them.

`recorder.mjs` is what established that the reverse proxy was forwarding over-long transcripts
verbatim. `capture-request.ps1` is what localised the Hermes integration's failure to its own
provider routing rather than to Sakur4 — the request never reached the proxy at all.
`reuse-probe.mjs` exists because two earlier scripts produced a contradiction: prompts with the
same measured token-level common prefix got opposite reuse results, so the decision involves
something other than the prefix.

### Written up rather than scripted

Two results needed a narrative, because the finding was in the analysis rather than in a
pass/fail:

- [proxy-rewrite.md](proxy-rewrite.md) — the reverse proxy's rewrite path: three defects, what
  caused each, and the wrong turns that found them. Includes the two assertions that had to be
  corrected because they failed for their own reasons.
- [proxy-harness.md](proxy-harness.md) — a real harness through the proxy, and the configuration
  rule that came out of it: do not run the proxy and the OMP extension at once.

`verify.mjs` at the repository root runs the automatable subset of all of this in one command,
and reports skipped separately from passed — because a run that skipped its live-server checks
is not a green run.
