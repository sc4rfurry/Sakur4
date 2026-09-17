# sakur4d

The [Sakur4](https://github.com/sc4rfurry/Sakur4) daemon: an MCP server that gives any
coding agent a persistent, self-curating memory and a cache-coherent context
window.

## Install

```bash
cargo install sakur4d
```

Or download a prebuilt binary from the
[releases page](https://github.com/sc4rfurry/Sakur4/releases).

## Use it from your harness

`config` prints ready-to-paste configuration with this binary's absolute path and
store baked in:

```bash
sakur4d config hermes          # ~/.hermes/config.yaml
sakur4d config claude          # claude_desktop_config.json
sakur4d config claude-code     # one-line CLI registration
sakur4d config generic-http    # for anything that connects to a URL
sakur4d config generic-stdio   # for anything that spawns a child process
```

Then run it:

```bash
sakur4d serve                          # stdio (default)
sakur4d serve --transport http --bind 127.0.0.1:8765
```

## See what it does without a harness

```bash
sakur4d demo --db :memory:
```

That walkthrough needs no GPU, model, or network. It drives a session past its
context budget, shows the eviction plan the engine chose, and prints the cache
verdict for the resulting boundary.

## What it exposes

17 MCP tools, 4 resources, and 1 prompt, targeting spec 2026-07-28:

- `memory.commit_episode`, `memory.pin`, `memory.recall`, `memory.fold`,
  `memory.unfold`, `memory.recall_fold`, `memory.staleness`
- `code.get_repo_map`, `code.query_symbol`, `code.impact_of_change`
- `session.snapshot`, `session.restore`
- `context.receipt`, `context.plan_eviction`, `context.record_usage`
- `sakur4.status`, `sakur4.dream`

## Other commands

```bash
sakur4d doctor              # what backend and cache capabilities were detected
sakur4d index <path>        # build the Repo Cortex index
sakur4d repo-map --budget 2000
sakur4d impact <symbol>
sakur4d plan <session>      # show an eviction decision before applying it
sakur4d receipt <session>   # where the context budget went, and the cache verdict
```

## License

Apache-2.0. See [LICENSE](https://github.com/sc4rfurry/Sakur4/blob/main/LICENSE).
