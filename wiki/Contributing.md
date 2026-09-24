# Contributing

> Sakur4 is a small project with strong opinions about **evidence**. This page gets you from a clone to
> a merged pull request, and tells you the invariants that will get a change rejected if you break them.

---

## Getting set up

```bash
git clone https://github.com/sc4rfurry/Sakur4
cd Sakur4
cargo build --workspace          # first build is slow — tree-sitter and SQLite compile from source
cargo test --workspace
node verify.mjs                  # the same checks CI runs, on your machine
```

**Requirements:** Rust (edition 2024, stable), Node 20+ for the verifier, Python 3.11+ only if you are
touching the Hermes engine, and OpenSSL development files only if you are building the `encryption`
feature.

**The first build takes several minutes** because `rusqlite` bundles SQLite and the tree-sitter grammars
compile from C. Subsequent builds are fast.

---

## Before opening a pull request

```bash
cargo fmt --all
cargo clippy --workspace --all-targets --all-features   # CI runs this with -D warnings
cargo test --workspace --all-targets
node verify.mjs
```

**`-D warnings` is not a suggestion.** A deprecation in a dependency is a build failure here, which is
how the project noticed that `rmcp` 3.4.1 deprecated two types it was using.

---

## The invariants worth knowing

These are enforced structurally — by the database schema, by types, or by a guard — rather than by
convention. Breaking one is not a style violation; it is a bug.

### The Episodic Stream is append-only

`UPDATE` and `DELETE` on `episodic_stream` are blocked by database triggers. A correction is a new row
that references the corrected one. Graduated eviction acts on **live-context inclusion**, never on the
stored text — which is why an evicted-then-recalled episode is bit-identical to the original.

### A model may not write to the Symbolic Ledger

Facts in the Ledger are extracted deterministically by parsing. No model output enters it, which is what
makes the Ledger trustworthy. If you are tempted to let a summary write a fact, that is the one thing the
design exists to prevent.

### Every Semantic Atlas entry must anchor to a fact

Enforced by a `NOT NULL` anchor plus `CHECK` constraints. Staleness is resolved **at read time** by
comparing hashes — never by trusting a value written at write time.

### Pinned anchors are exempt from compaction, and refusal is visible

`render_anchor_block` returns `BudgetOverflow` when the anchors alone cannot fit. That is FR-4's
*"visible warning rather than silent drop"*. **All four prompt-building paths must route through that
function** — there is a check for it, because all four used to bypass it with a `join`.

### Cross-project reads are a request, not an accident

Recall is scoped to the project the daemon was started for. There is a `project_id` parameter for asking
across projects deliberately; omitting it must not widen the search.

---

## Testing expectations

**A test that cannot fail is worse than no test.** This project has repeatedly found checks that passed
while the thing they guarded was broken, so the rule is: **verify your test fails when the code is
wrong**, then keep it.

That applies to documentation guards especially. If you add a check under `docs/verification/`, make it
fail on purpose first and say in the code what it looked like.

**Prefer a test that runs the real thing.** The gateway tests speak MCP over a real listener to a real
daemon. The verification scripts spawn a real process. A unit test of a function that nothing calls is
how several real defects stayed hidden for a long time.

---

## Commit messages

Explain **what was wrong**, not what you changed. The git log is the project's design record, and the
most useful entries are the ones that say why the previous version was wrong — including when the
previous version was mine.

For anything that fixes a defect: say what the symptom looked like, and what the actual cause was, in
that order. Several entries in this project's history are worth reading precisely because the first
diagnosis was wrong and the entry says so.

---

## Reporting bugs

Open an issue with: what you ran, what you expected, what happened, and the version
(`sakur4d --version`). If it involves the store, `sakur4d doctor` output is the single most useful thing
you can attach — it reports the schema version, backend capabilities, journal mode and counts.

If it is a **security** problem, do not open a public issue. See [Security](Security).

---

## Where help is most useful

| Area | Why it matters |
|---|---|
| **The ordering defect** | Documented in [Limitations](Limitations) with eight failed attempts and an exact statement of what the next one needs |
| **A second backend** | Everything is verified against one llama.cpp build. Another build's capability probe would be genuinely new information |
| **LoCoMo / Endurance Benchmark** | The A/B is narrow. A standardised benchmark would test the central claim properly |
| **A live Hermes session** | The engine passes 44 contracts; driving it with a real model has not succeeded |

---

<sub>[← Back to Home](Home) · [All pages](Home#where-to-go-next)</sub>
