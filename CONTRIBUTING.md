# Contributing to Sakur4

Thanks for considering a contribution. This document is short on ceremony and
specific about the parts that are easy to get wrong.

## Getting set up

```bash
git clone <your fork>
cd sakur4
cargo build
cargo test --workspace
```

Rust 1.94 or newer. No GPU, model, or network access is required: the test suite
runs against an embedded backend that simulates a llama.cpp slot, so everything
works on any machine.

Useful commands while developing:

```bash
cargo test -p sakur4-core --lib          # fast unit tests
cargo test -p sakur4d --test stdio_transport   # spawns the real binary
cargo run -p sakur4d -- demo --db :memory:     # end-to-end walkthrough
cargo run -p sakur4d -- doctor                 # what did it detect?
```

## Before opening a pull request

CI enforces all four of these, but there is now one command that runs everything:

```bash
node verify.mjs                              # everything this machine can run
node verify.mjs --upstream http://host:8080  # add the live llama.cpp checks
node verify.mjs --quick                      # skip the slow benchmarks
node verify.mjs --only rust,hermes           # a subset, by group or check id
node verify.mjs --list                       # what exists, and what each needs
```

It reports **skipped separately from passed**, because those are different things: a run
that skipped its live-server checks is not a green run. `--require-all` makes a skip fail,
which is what CI uses.

The individual commands, if you would rather drive them yourself:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets
cargo test --workspace --all-targets
cargo doc --workspace --no-deps
```

`RUSTFLAGS=-D warnings` is set in CI, so a warning is a failure there. Please keep
the tree warning-free rather than leaving that to a reviewer.

## The invariants worth knowing before you change anything

Sakur4 has a small number of structural guarantees. They are enforced by the code
rather than by review, and a change that breaks one will usually break a test with
a confusing message. Knowing them up front makes the codebase much easier to work
in.

**The Symbolic Ledger is LLM-free.** `SymbolicFact` has exactly one constructor and
it takes a `FactSource`, a closed enum of deterministic extractors. Do not add a
constructor that accepts arbitrary text, and do not let `memory/symbolic.rs` import
`embed`, `llama`, or `consolidate`. It genuinely does not today; keeping it that
way is what makes the anti-hallucination guarantee real.

**Recorded content is immutable.** `UPDATE` and `DELETE` on `episodic_stream`
content are blocked by database triggers. Eviction changes `eviction_tier`, which
changes how an episode is *rendered*, never what is stored. If you need to change
what an episode says, add a new episode referencing the old one.

**Anchors are never eviction candidates.** Eviction selects from episodes; anchors
live in a different table. If a change makes it possible to evict a pinned
constraint, that is a bug even if the tests pass.

**Everything is measured through one tokenizer.** Budget decisions and printed
numbers must come from the same `TokenCounter`. Estimating in one place and
measuring in another has already caused a real bug here: the eviction engine
decided "relaxed" while the receipt printed a window three-quarters full.

**The MCP surface is a contract.** Tool names, argument shapes, and result shapes
are what harnesses depend on. Adding a tool is a minor change. Renaming a field or
changing a type is a breaking change and needs a migration note in `CHANGELOG.md`.

## On the cache-coherence code specifically

`crates/sakur4-core/src/evict.rs` (`boundary_and_prefix`) and
`crates/sakur4-core/src/cache/` are the most delicate part of the project, because
getting them wrong is silent. The order of operations is the design: ask the cache
layer where the boundary *can* fall, then evict after it. Deciding evictions first
and asking the cache afterwards produces a boundary at token 0, which no checkpoint
can align to, and every compaction reports a full re-prefill — the exact failure
the project exists to remove, arrived at by its own machinery.

Three separate versions of this code were wrong, and each left every existing test
green. That is why `crates/sakur4-core/tests/cache_coherence.rs` states the claim as
executable contracts. If you change this area, expect to change those tests, and be
suspicious of a change that does not require it.

The same applies to `PromptParts`: it is the single prompt assembler, deliberately,
so that the eviction engine and the receipt cannot disagree about where things are.

## Testing expectations

- **Bug fixes come with a regression test.** The test should fail before the fix.
  Several tests in this repository are named after the specific mistake they guard,
  which is more useful to a future reader than `test_eviction_3`.
- **New tools come with an over-the-wire test.** `crates/sakur4d/tests/` drives the
  real binary. A handler-level unit test skips JSON-RPC framing, protocol
  negotiation, and argument shapes — which is where the bugs actually were. One
  unconstrained output schema once made a real harness reject the entire tool
  catalog.
- **Prefer asserting the property to asserting the value.** A test that says "the
  preserved prefix is a byte-prefix of what the server is sent" catches more than
  one that says "the cut is at 4034".
- **Comments should explain why.** What the code does is usually visible; why it is
  shaped that way, and what went wrong when it was not, is not.

## Commit messages

Write them for someone reading `git log` in a year. If a change fixes a subtle bug,
say what the failure mode was — that is the part nobody can reconstruct later.

## Reporting bugs

Include:

- What you expected and what happened.
- The output of `sakur4d doctor` (it reports the resolved backend, the detected
  cache capabilities, and the store's contents).
- `RUST_LOG=debug` output if the problem concerns eviction or cache behaviour.
- Whether a harness is involved, and which.

If the bug is about a compaction being slow or expensive, `context.receipt` is the
place to look first: it reports where the token budget went and whether the prompt
had to be re-prefilled.

## Security

Please do not open a public issue for a security problem. See
[SECURITY.md](SECURITY.md).

## License

Contributions are accepted under the Apache License 2.0. See [LICENSE](LICENSE).
By opening a pull request you confirm you have the right to submit the work under
those terms.
