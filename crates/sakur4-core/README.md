# sakur4-core

The engine behind [Sakur4](https://github.com/sc4rfurry/Sakur4) — a cache-coherent
memory and context operating system for local coding and research agents.

This crate is published so the `sakur4d` daemon has a home on crates.io. It is a
working library — the same one the daemon and its test suite use — but the Rust API
is not yet a stability promise. **The stable contract is the MCP tool surface**,
which lives in the `sakur4d` crate and is versioned separately in spirit: tool
names, argument shapes, and result shapes are what harnesses depend on.

## What is in it

| Module | What it does |
|---|---|
| `memory` | The dual-track Memory Fabric: append-only Episodic Stream, deterministic Symbolic Ledger, anchored Semantic Atlas, Anchor Set, Dependency Graph |
| `evict` | The Graduated Eviction Engine: four tiers, dependency-graph-aware scoring, `fold`/`unfold` |
| `cache` | The Cache-Coherence Layer: checkpoint-aligned eviction boundaries, snapshot/restore |
| `llama` | The pluggable backend trait plus llama.cpp, embedded and null implementations |
| `repo` | Repo Cortex: tree-sitter extraction, call/import graph, token-budgeted repo map |
| `recall` | Hybrid retrieval with staleness-aware reranking |
| `consolidate` | The Idle Consolidator: promotion, staleness regeneration, cold archival |
| `provider_cache` | Prompt-cache accounting for hosted providers |
| `receipt` | The Context Ledger Receipt |
| `prompt` | The single prompt assembler every budget decision uses |

## The idea in one paragraph

A harness that compacts by summarising produces a prompt whose first token differs
from the previous one, so llama.cpp's longest-common-prefix slot matching finds
nothing and the whole compacted context is re-prefilled. The operation whose purpose
was to make the session cheap becomes the most expensive thing in it. Sakur4 makes
the compaction decision with the inference server's own KV-cache checkpoints in
view: it asks the Cache-Coherence Layer where a boundary *can* fall, then evicts
after it, so the surviving prompt head stays a prefix of what the server holds.

On a hosted provider there is no slot API, so the same accounting is done from the
provider's reported cache-token counts instead.

## Example

```rust
use sakur4_core::{Engine, EngineConfig};

# async fn run() -> Result<(), Box<dyn std::error::Error>> {
let engine = Engine::open(EngineConfig {
    db_path: ":memory:".into(),
    backend: "embedded".into(),   // no GPU, no model, no network
    ..Default::default()
}).await?;

let status = engine.status().await?;
println!("backend: {} ({})", status.backend_name, status.cache_summary);
# Ok(())
# }
```

## Guarantees that are structural, not aspirational

- The Symbolic Ledger has one constructor, and it requires naming the deterministic
  parser that produced the fact. There is no path from a model into it.
- `UPDATE` and `DELETE` on recorded episode content are blocked by database
  triggers, so "an evicted episode recalls byte-identically" cannot regress.
- Anchors are exempt from eviction because eviction selects from episodes, which
  live in a different table.
- Library code contains zero `unwrap`, `expect`, or `panic!` paths outside tests.

## License

Apache-2.0. See [LICENSE](https://github.com/sc4rfurry/Sakur4/blob/main/LICENSE).
