## What this changes

<!-- One or two sentences. If it fixes a bug, say what the failure mode was — that
     is the part nobody can reconstruct from the diff later. -->

## Why

<!-- The reasoning. If there was a simpler alternative you rejected, say why. -->

## Checklist

- [ ] `cargo fmt --all` (no diff)
- [ ] `cargo clippy --workspace --all-targets --all-features` (no warnings — CI sets `-D warnings`)
- [ ] `cargo test --workspace --all-targets` passes
- [ ] `cargo doc --workspace --no-deps` is warning-free
- [ ] A bug fix comes with a regression test that fails without the fix
- [ ] A new or changed MCP tool comes with an over-the-wire test in `crates/sakur4d/tests/`
- [ ] `CHANGELOG.md` updated if this changes the MCP surface or user-visible behaviour

## Invariants

If this touches any of the following, say how the guarantee is preserved:

- [ ] The Symbolic Ledger still cannot be written from a model (no new constructor,
      no new import of `embed`/`llama`/`consolidate` into `memory/symbolic.rs`)
- [ ] Recorded episode content is still immutable (no new `UPDATE` path)
- [ ] Anchors are still not eviction candidates
- [ ] Budget decisions and printed numbers still go through one `TokenCounter`
- [ ] Cache-coherence boundary decisions still happen *before* eviction selection

<!-- Not every box applies to every change. Delete the ones that do not, rather
     than ticking them without thinking. -->

## How this was verified

<!-- What you ran, and what you observed. "Tests pass" is fine if that is what
     happened; if you exercised something by hand, say what you saw. -->
