Five ways in, so a harness needs no particular capability to be reached — and an explicit note on which of them are **verified** and which are not.

---

**On this page:** [What is verified](#what-is-verified) · [MCP — any client](#mcp--any-client) · [Oh My Pi](#oh-my-pi--native-extension) · [Hermes](#hermes--a-contextengine-not-just-tools) · [Agent Skill](#agent-skill--no-mcp-no-extension) · [Reverse proxy](#anything-else--an-openai-compatible-reverse-proxy)

All five reach the same daemon and the same memory, and **you can use more than one**.

## What is verified

Stated up front, because the difference between "supported" and "seen working" is the whole value of this table.

| Route | Verified | Not verified |
|---|---|---|
| **MCP over stdio** | `stdio_transport.rs` spawns the **real binary** and speaks JSON-RPC over its pipes. `hermes mcp test sakur4` discovers all **17** tools against a real Hermes install. | — |
| **MCP over HTTP** | `gateway.rs` drives the tool surface over a **live HTTP listener** using the SDK's own client. | No MCP reference-client conformance run. |
| **Oh My Pi** | Native tools driven end-to-end by a **live model**; verified against OMP **18.2.x** (installed build 18.2.11). | **The OMP compaction hook has never fired for real** — forcing OMP past its context limit is separate work. |
| **Hermes** | Engine registers, is instantiated by a real Hermes install, talks to a live daemon, and passes **44 contracts**. Its MCP transport discovers all 17 tools. | **No live agent session through Hermes** — its own tool selection is untested, and a session driving a model has not succeeded (Hermes provider routing). |
| **Agent Skill** | Ships in every release archive and is installed by `install.sh` into `~/.agents/skills/`. | Nothing exercises it in CI: the `harness` group needs OMP or the Hermes CLI, and **CI has neither**. |
| **Reverse proxy** | Verified against the OMP harness — the full comparison is in `docs/verification/proxy-harness.md`. | — |

`verify.mjs` reports **skipped separately from passed**, so a run that skipped its live checks is not a green run.

## MCP — any client

The baseline route, and the one that needs nothing beyond the daemon. `config` prints ready-to-paste configuration with **this binary's absolute path and store baked in**, so there is no placeholder to forget:

```bash
sakur4d config hermes          # ~/.hermes/config.yaml
sakur4d config claude          # claude_desktop_config.json
sakur4d config claude-code     # one-line CLI registration
sakur4d config generic-http    # anything that connects to a URL
sakur4d config generic-stdio   # anything that spawns a child process
```

`[HARNESS]` defaults to `hermes`, and `--binary` overrides the path that gets baked in.

| Transport | How the harness reaches it | Use it when |
|---|---|---|
| **stdio** | Spawns `sakur4d` and speaks JSON-RPC over its pipes. | One harness; no port to manage. **This is every MCP client**, and it is the default. |
| **streamable HTTP** | Connects to a URL. | Several sessions sharing one store, or a harness on another machine. |

```bash
sakur4d serve                                            # stdio — the default
sakur4d serve --transport http --bind 127.0.0.1:8765     # shared
```

Over HTTP, requests carry the revision in a per-request `_meta` block, and the SEP-2243 headers `MCP-Protocol-Version` and `Mcp-Method` — plus `Mcp-Name` for `tools/call` — are required.

**Configuration to paste** — `sakur4d config generic-stdio` and `sakur4d config generic-http` produce exactly these, with real paths:

```json
{
  "mcpServers": {
    "sakur4": {
      "command": "/absolute/path/to/sakur4d",
      "args": [
        "--db", "/absolute/path/to/sakur4.db",
        "--project-root", "/absolute/path/to/project",
        "serve", "--transport", "stdio"
      ]
    }
  }
}
```

For a shared HTTP server instead, start it once in its own terminal and point the client at the URL:

```bash
sakur4d --db /absolute/path/to/sakur4.db serve --transport http --bind 127.0.0.1:8765
# client URL:  http://127.0.0.1:8765/
```

> **Bind to `127.0.0.1` unless you mean to expose the store on a network. There is no authentication** — localhost binding *is* the control, and `--bind 0.0.0.0` exposes the entire Memory Fabric, including writes, to anyone who can reach the port.

## Oh My Pi — native extension

For OMP there is **no MCP client**. It needs a native TypeScript extension instead — which turns out to be an advantage, because an extension can see inside the agent loop and therefore reach hooks a tool provider cannot.

```bash
node integrations/omp-plugin/install.mjs
```

The installer copies the plugin into `~/.omp/plugins/node_modules/omp-sakur4`, declares it in `~/.omp/plugins/package.json`, enables it in `omp-plugins.lock.json`, and installs the Agent Skill into `~/.agents/skills/`. Options: `--no-skill`, `--skill-only`, `--dir <omp home>`, `--uninstall`.

Then **restart OMP** and ask it to list its `sakur4_` tools — there should be **nine**: `sakur4_commit`, `sakur4_pin`, `sakur4_recall`, `sakur4_symbol`, `sakur4_impact`, `sakur4_fold`, `sakur4_unfold`, `sakur4_receipt`, `sakur4_status`.

| Hook | What Sakur4 does with it |
|---|---|
| `session_start` | Probes the daemon once; reports a missing binary before ten turns go unrecorded; live counts in the status bar. |
| `before_agent_start` | Injects the working preamble **once** per session — the instructions that make a model actually pin and fold. |
| `context` | Retrieves memory for the prompt, capped, and **reports its own token cost** so the budget stays honest. |
| `message_end` | Forwards provider token usage every turn, automatically. |
| `session_before_compact` | Replaces blind summarisation with Sakur4's planned eviction. |
| `session_shutdown` | Reports stale summaries, because the next session inherits them. |
| `resources_discover` | Contributes the bundled Agent Skill. |

There is also a `/sakur4` command: `status`, `receipt`, `recall <query>`, `pin <text>`, `index [path]`, `map [budget]`, `staleness`, `dream`.

**Compaction falls back on purpose.** If the daemon is unreachable, the plan is empty, or the plan would not actually reduce the context, the hook returns nothing and OMP performs its normal compaction.

**Two install traps the installer avoids.** `omp install <path>` **symlinks**, which fails on Windows with a bare `EPERM: operation not permitted, symlink` unless Developer Mode is on — the installer copies instead. And OMP's plugin loader **silently skips a lockfile entry** that is neither declared in `~/.omp/plugins/package.json` **nor** a symlink, reporting it only as `skipping stale lockfile entry` in a log — so the plugin appears in `omp plugin list` *and* passes `omp plugin doctor` while never actually loading. Writing both files is the fix.

**Configuration to paste** — the installer writes these for you; this is what to check if the tools do not appear:

```bash
# 1. Put sakur4d on PATH (`cargo install sakur4d`), or name it explicitly:
export SAKUR4_BIN=~/.cargo/bin/sakur4d

# 2. Install, then restart OMP.
node integrations/omp-plugin/install.mjs
```

| Variable | Default | Meaning |
|---|---|---|
| `SAKUR4_RETRIEVE` | `true` | Inject retrieved memory before each turn. |
| `SAKUR4_REPORT_USAGE` | `true` | Report provider usage automatically. |
| `SAKUR4_OWN_COMPACTION` | `true` | Take over compaction. |
| `SAKUR4_RECALL_BUDGET` | `1200` | Approximate token cap on injected memory. |
| `SAKUR4_SESSION` | `omp-<dir name>` | Session id. |
| `SAKUR4_PLUGIN_LOG` | unset | Append lifecycle diagnostics to this file. |

**Requirements:** Oh My Pi **18.2.x**, `sakur4d` 0.1.0 or newer, Node.js 18+ (OMP bundles its own runtime). **The plugin declares no version floor at all** — its `package.json` carries `"@earendil-works/pi-coding-agent": "*"` as an optional peer dependency, the extension API is undocumented, and a minor bump can change it without notice. `--no-extensions` is how to tell whether a failure is OMP's or this plugin's.

## Hermes — a ContextEngine, not just tools

Hermes has its own compaction path, so exposing MCP tools is not enough: its summariser still runs. `integrations/hermes-plugin/` **replaces** it, which also closes a loop MCP alone cannot — `update_from_response` receives the provider's token accounting on every call, so prompt-cache behaviour is measured automatically instead of reported by hand.

| Hermes hook | What this engine does with it |
|---|---|
| `update_from_response(usage)` | Forwards the provider's token accounting — including cache read/write counts — to Sakur4 on **every** call. |
| `should_compress` | Fires at the same threshold Hermes would, so behaviour stays predictable. |
| `compress` | Commits every message to the append-only stream, asks Sakur4 what to evict, and replaces exactly those messages with a marker. |
| `select_context` | Injects the Anchor Set into every request, **after** the system prompt. |
| `get_tool_schemas` / `handle_tool_call` | Exposes `sakur4_recall`, so the model can look something up rather than reconstruct it. |
| `prune_tool_results_only` | Commits but **prunes nothing** — Sakur4's plan already treats re-runnable tool results as its first eviction candidates, and pruning here as well would evict twice for one saving. |
| `__deepcopy__` | Copies budget state and builds a fresh client, because Hermes deep-copies the engine for sub-agents. |

**Every daemon call is best-effort.** If `sakur4d` is not running, `compress()` returns the messages **unchanged** and Hermes handles the overflow with its own fallback. A context engine that breaks a session when its sidecar is down is worse than no context engine.

Three enabling details, each of which cost time to find:

1. **`context.engine` must not be the default.** The selector returns `None` immediately when the engine name is `compressor`, so a plugin is never consulted. Any other name takes the plugin path.
2. **A plugin must be *enabled*, not merely discovered.** `hermes plugins list` shows `sakur4 … not enabled` and the loader agrees until `hermes plugins enable sakur4` has run.
3. **The ContextEngine loader reads the repo, not `HERMES_HOME`.** The general plugin system at `$HERMES_HOME/plugins/` can be redirected; that is the path this engine uses.

**Configuration to paste:**

```bash
# 1. Install the plugin.
cp -r integrations/hermes-plugin "$LOCALAPPDATA/hermes/plugins/sakur4"
hermes plugins enable sakur4          # discovery and activation are separate steps
hermes plugins list                   # should show `sakur4` with source `user`

# 2. Start the daemon the engine talks to.
sakur4d serve --transport http --bind 127.0.0.1:8770 --context-window <your model's window>
```

```yaml
# ~/.hermes/config.yaml
context:
  engine: sakur4
```

| Variable | Default | Meaning |
|---|---|---|
| `SAKUR4_URL` | `http://127.0.0.1:8765` | Where the daemon is listening. |
| `SAKUR4_THRESHOLD_PERCENT` | `0.75` | Fraction of the window at which compaction fires. |
| `SAKUR4_BIN` | searched | Path to `sakur4d`, using the same search order as the OMP extension. |
| `SAKUR4_SESSION` | `hermes` | Session id. |

**Pass `--context-window`, and make it small enough that your history exceeds the trigger.** Without it the daemon plans against the backend's reported window — the embedded backend simulates 32,768 — and a plan that correctly reclaims nothing reads as "the engine cannot compact" when it is the *test* that has not applied enough pressure.

> **No live Hermes model session has succeeded**, and the cause is Hermes' provider resolution rather than this engine: a `--model` override that does not resolve **fails silently**, falling back to the config's cloud default and reporting *its* credentials. `integrations/hermes-plugin/LIVE-TESTING.md` records exactly where it stopped so finishing it does not mean repeating that search. Check `hermes status` before blaming anything downstream.

## Agent Skill — no MCP, no extension

`skills/sakur4/` is a portable [Agent Skills](https://agentskills.io/specification) package: a `SKILL.md` plus a **dependency-free** Node CLI over the daemon.

`~/.agents/skills/` is the standard location, so **OMP, Claude Code, Codex and pi all pick it up with no further configuration**. Progressive disclosure means only the description sits in context until a task matches.

The CLI locates `sakur4d` across install layouts, defaults the store to `~/.sakur4/sakur4.db`, and spawns with `shell: false` — so recorded content may contain quotes, newlines or backticks intact. That matters when the primary use is committing the user's words verbatim.

**Configuration to paste:**

```bash
# Install just the skill, without the OMP extension.
node integrations/omp-plugin/install.mjs --skill-only    # → ~/.agents/skills/

# Or take it straight from a checkout / the release archive.
node scripts/sakur4.mjs doctor
node scripts/sakur4.mjs doctor --bin /path/to/sakur4d
```

Every skill command accepts `--session`, `--bin` and `--db`, so the environment variables are conveniences rather than requirements. The CLI's commands are `doctor`, `commit`, `pin`, `recall`, `receipt`, `usage`, `fold`, `unfold`, `index`, `map`, `symbol`, `impact`, `staleness` and `dream`.

```bash
node scripts/sakur4.mjs commit --role user --content "refactor the auth module"
node scripts/sakur4.mjs pin --kind task_contract --content "keep the public API of src/auth.rs backward compatible"
node scripts/sakur4.mjs recall --query "how did we decide to handle retries" --k 5
```

**What the skill deliberately does not do:** it does not replace compaction or decide what leaves the context window — that is the harness's job, and Sakur4's eviction engine only ever advises. It does not call a model. And it does not claim to know something it did not store: if `recall` returns nothing, the honest next step is to read the file.

## Anything else — an OpenAI-compatible reverse proxy

A harness with **none** of the above still works. Point it at the proxy instead of at `llama-server` and nothing else changes.

Requests are forwarded untouched — **every unrecognised route included**, so a harness calling an endpoint this build has never heard of gets the upstream's own answer rather than a 404 from Sakur4. A transcript that exceeds the window is trimmed on the way through, with a marker left in place of the removed turns, and the provider's token accounting is recorded from the response the proxy already had to read.

```bash
sakur4d proxy --bind 127.0.0.1:8090 --upstream http://127.0.0.1:8080
# harness base URL: http://127.0.0.1:8090/v1
```

**Configuration to paste:**

```bash
# 1. Measure first. Forward everything unchanged, record only.
sakur4d proxy --bind 127.0.0.1:8090 --upstream http://127.0.0.1:8080 --observe-only

# 2. Then let it act on real traffic.
sakur4d proxy --bind 127.0.0.1:8090 --upstream http://127.0.0.1:8080
```

`--observe-only` is the safe way to see what the proxy *would* have done on your real traffic before letting it act. `--session` names the session reported to the Memory Fabric.

> **Do not run the proxy and the OMP extension at the same time.** Both manage context, and a turn gets managed twice — OMP hangs before sending its first request. Use the proxy **or** the extension. `--no-extensions` disables the extension for a proxied session.

---

<sub>[← Back to Home](Home) · [All pages](Home#where-to-go-next)</sub>
