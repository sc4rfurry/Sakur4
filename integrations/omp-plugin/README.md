# Sakur4 for Oh My Pi

A native Oh My Pi extension that gives OMP a persistent memory and a
cache-coherent context window.

OMP has no MCP client, so Sakur4 cannot reach it the way it reaches other
harnesses. This extension talks to the same `sakur4d` daemon directly and, because
an extension can see inside the agent loop, uses hooks that a tool provider cannot:
it injects a working preamble, retrieves memory before each turn, reports provider
usage automatically, and takes over compaction.

## Install

```bash
cargo install sakur4d                       # or download a release binary
node integrations/omp-plugin/install.mjs    # from a Sakur4 checkout
```

Then restart OMP and ask it to list its `sakur4_` tools — there should be nine.

The installer copies the plugin into `~/.omp/plugins/node_modules/omp-sakur4`,
declares it in `~/.omp/plugins/package.json`, enables it in
`omp-plugins.lock.json`, and installs the Agent Skill into `~/.agents/skills/`
where OMP, Claude Code, Codex and pi all read from.

Options: `--no-skill`, `--skill-only`, `--dir <omp home>`, `--uninstall`.

### Why not `omp install`

`omp install <path>` symlinks the package, which on Windows fails with a bare
`EPERM: operation not permitted, symlink` unless Developer Mode is on. The
installer does the same work by copying instead.

There is a second trap it avoids. OMP's plugin loader skips any lockfile entry that
is neither declared in `~/.omp/plugins/package.json` **nor** a symlink — and it
reports this only as `skipping stale lockfile entry` in a log. The plugin then
appears in `omp plugin list` *and* passes `omp plugin doctor`, while never actually
loading. Writing both files is the fix, and it is the difference between a plugin
that looks installed and one that is.

## What it adds

### Nine tools

| Tool | What it does |
|---|---|
| `sakur4_commit` | Records a turn or tool result in append-only memory. Structured output (JSON, CSV, diffs, exit codes) is parsed into facts. |
| `sakur4_pin` | Pins a rule, correction or requirement so compaction cannot remove it. |
| `sakur4_recall` | Searches memory. Stale summaries come back with their source's current value. |
| `sakur4_symbol` | A symbol's current signature from the parser index — cannot be stale. |
| `sakur4_impact` | Every call site that depends on a symbol, transitively. |
| `sakur4_fold` | Opens an isolated sub-context for a multi-step subtask. |
| `sakur4_unfold` | Closes it, collapsing the trace to a one-line result. |
| `sakur4_receipt` | Where the context budget went, and the cache verdict. |
| `sakur4_status` | Resolved backend, detected cache capabilities, store counts. |

### The `/sakur4` command

```
/sakur4 status              backend, episode and anchor counts
/sakur4 receipt             this session's token accounting
/sakur4 recall <query>      search memory
/sakur4 pin <text>          pin a constraint
/sakur4 index [path]        build the code graph
/sakur4 map [budget]        structural outline of the repository
/sakur4 staleness           summaries that no longer match their source
/sakur4 dream               run a memory-maintenance pass
```

### Hooks

| Hook | What it does |
|---|---|
| `session_start` | Probes the daemon once, so a missing binary is reported before ten turns have gone unrecorded, and puts live counts in the status bar. |
| `before_agent_start` | Injects the preamble once per session — the instructions that make a model actually pin and fold. |
| `context` | Retrieves memory for the current prompt and prepends it, capped, and reports its own token cost so the budget stays honest. |
| `message_end` | Forwards the provider's token usage every turn, so prompt-cache behaviour is measured automatically rather than only when asked. |
| `session_before_compact` | Replaces blind summarisation with Sakur4's planned eviction. |
| `session_shutdown` | Reports summaries that have gone stale, because the next session inherits them. |
| `resources_discover` | Contributes the bundled Agent Skill. |

## The compaction behaviour, which is the interesting part

OMP compacts by summarising. A summary produces a prompt with no prefix in common
with the previous one, so the provider's prompt cache is invalidated from the
rewrite point onward and the next request pays full price for history it had
already paid for. Hermes' documentation names this as the strongest argument
against per-turn compaction, and it applies here too.

When OMP is about to compact, this extension asks `sakur4d` for an eviction plan
instead, applies it, and builds the replacement summary from what the engine kept
plus the pinned anchors — then reports the cache verdict for the boundary it chose.

**It falls back on purpose.** If the daemon is unreachable, the plan is empty, or
the plan would not actually reduce the context, the hook returns nothing and OMP
performs its normal compaction. A bespoke compaction that is worse than the default
is not an improvement.

## Configuration

Everything is optional. The defaults work without configuration.

| Variable | Default | Meaning |
|---|---|---|
| `SAKUR4_BIN` | searched | Path to `sakur4d`. Searched: `SAKUR4_BIN`, `~/.cargo/bin`, beside `node`, `~/.sakur4/bin`, then `PATH`. |
| `SAKUR4_DB` | `~/.sakur4/sakur4.db` | Memory store. |
| `SAKUR4_SESSION` | `omp-<dir name>` | Session id. |
| `SAKUR4_BACKEND` | daemon default | `auto`, `embedded`, `none`, or a llama.cpp base URL. |
| `SAKUR4_RETRIEVE` | `true` | Inject retrieved memory before each turn. |
| `SAKUR4_REPORT_USAGE` | `true` | Report provider usage each turn. |
| `SAKUR4_OWN_COMPACTION` | `true` | Take over compaction. |
| `SAKUR4_RECALL_BUDGET` | `1200` | Approximate token cap on injected memory. |
| `SAKUR4_PLUGIN_LOG` | unset | Append lifecycle diagnostics to this file. |

### When something does not work

Set `SAKUR4_PLUGIN_LOG` to a path and restart OMP. Every lifecycle step appends a
line, so "did the extension even load" is answerable in one look:

```bash
SAKUR4_PLUGIN_LOG=/tmp/sakur4.log omp
cat /tmp/sakur4.log
```

A plugin that silently does nothing and a plugin that failed to load are otherwise
indistinguishable, because OMP surfaces extension-load errors only to a TTY.

## Requirements

- Oh My Pi 18.2.x. Verified against the installed 18.2.11. The extension API is undocumented and
  changes between minor versions, so a build older or newer than 18.2 may not load. **This line used
  to say "18.1.17 or newer"** — that named the version the API was first reverse-engineered from,
  not one anything is tested against, and nothing enforces it: `package.json` declares
  `"@earendil-works/pi-coding-agent": "*"`.
- Node.js 18+ (OMP bundles its own runtime).
- `sakur4d` 0.2.0 or newer. No GPU, model or network needed — the daemon's
  embedded backend simulates a llama.cpp slot, so the extension is fully usable
  without a local model.

## License

Apache-2.0. See [LICENSE](../../LICENSE).
