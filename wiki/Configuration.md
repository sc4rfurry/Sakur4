Every flag with its default, every `SAKUR4_*` variable, the three backends, the eviction profiles, and worked configurations.

---

**On this page:** [Global flags](#global-flags) · [Per-command flags](#per-command-flags) · [Environment variables](#environment-variables) · [The three backends](#the-three-backends) · [Eviction profiles](#eviction-profiles) · [Common configurations](#common-configurations) · [Checking what resolved](#checking-what-resolved)

**Everything is optional. The defaults work.**

## Global flags

These apply to every subcommand, written either `--db <path>` or `--db=<path>`. Every one of them also reads an environment variable, shown in the `Env` column.

| Flag | Env | Default | Meaning |
|---|---|---|---|
| `--db <DB>` | `SAKUR4_DB` | `sakur4.db` | Path to the Memory Fabric store. |
| `--backend <BACKEND>` | `SAKUR4_BACKEND` | `auto` | `auto`, `embedded`, `none`, or an HTTP base URL. |
| `--project-root <PROJECT_ROOT>` | `SAKUR4_PROJECT_ROOT` | the working directory | Repository root for Repo Cortex. |
| `--embed-url <EMBED_URL>` | `SAKUR4_EMBED_URL` | unset | Local embedding endpoint (OpenAI-compatible). |
| `--embed-model <EMBED_MODEL>` | `SAKUR4_EMBED_MODEL` | `nomic-embed-text` | Embedding model name for that endpoint. |
| `--context-window <CONTEXT_WINDOW>` | — | unset | Context window to plan against. |
| `-v`, `--verbose` | — | off | Increase log verbosity (`-v`, `-vv`). |
| `-h`, `--help` / `-V`, `--version` | — | — | Help and version. |

**`--context-window` is deliberately optional rather than defaulted**, so the engine can tell "the user set this" from "nobody said". With a default the two are indistinguishable, and a user's explicit value loses to a backend's *simulated* answer — which is how this flag came to have no effect on the embedded backend while appearing to be accepted. **An explicit value wins.**

**`--db` resolves relative to the working directory**, which is correct for a command you run yourself. It is **not** correct for a path baked into a harness's configuration, because the harness picks the working directory — so `config` resolves it to an **absolute** path before printing it.

## Per-command flags

| Command | Flags and defaults |
|---|---|
| `serve` | `--transport` (`SAKUR4_TRANSPORT`, default `stdio`; accepts `stdio`, `http`, `http://host:port`, or a bare `host:port`) · `--bind` (default `127.0.0.1:8765`) · `--no-dream` · `--quiet-secs` (default `90`) · `--banner` |
| `proxy` | `--bind` (default `127.0.0.1:8090`) · `--upstream` (default `http://127.0.0.1:8080`) · `--session` · `--observe-only` |
| `config` | `[HARNESS]` (default `hermes`) · `--binary` (defaults to this executable) |
| `doctor` | `--refresh` |
| `index` | `[PATH]` (defaults to `--project-root` or the working directory) · `--full` |
| `repo-map` | `--budget` (default `2000`) · `--focus` · `--names` |
| `impact` | `<SYMBOL>` · `--depth` (default `4`) |
| `symbol` | `<QUALIFIED_NAME>` |
| `recall` | `<QUERY>` · `--k` (default `8`) · `--session` · `--include-folded` |
| `commit` | `<SESSION> <CONTENT>` · `--role` (default `user`) · `--tool` · `--slot` (default `0`) |
| `pin` | `<CONTENT>` · `--kind` (default `task_contract`) · `--session` |
| `anchors` | `--session` |
| `plan` | `<SESSION>` · `--slot` (default `0`) · `--apply` |
| `snapshot` | `--session` (default `default`) · `--slot` (default `0`) |
| `restore` | `<PATH>` · `--session` (default `default`) · `--slot` (default `0`) |
| `receipt` | `<SESSION>` · `--history` · `--limit` (default `20`) |
| `dream` | — |
| `staleness` | — |
| `demo` | `--repo` |
| `gen-key` | — |

**Two flags worth knowing by name.** `serve --no-dream` disables the Idle Consolidator, which is **on** by default — memory maintenance should not require opting in, and it refuses to run while any tracked slot is generating. `serve --banner` prints a startup banner to **stderr**; it is off by default because over stdio the client owns the process, and a harness that captures stderr would collect a banner on every session.

## Environment variables

### The daemon

| Variable | Default | Meaning |
|---|---|---|
| `SAKUR4_BIN` | searched | Path to `sakur4d`. |
| `SAKUR4_DB` | `sakur4.db` (daemon) · `~/.sakur4/sakur4.db` (skill) | Memory store. |
| `SAKUR4_SESSION` | derived | Session id. |
| `SAKUR4_BACKEND` | `auto` | `auto` · `embedded` · `none` · a llama.cpp base URL. |
| `SAKUR4_LLAMA_API_KEY` | unset | Sent as a **bearer token** to the inference server. Needed for any server that requires authentication — llama.cpp behind a reverse proxy, or with `--api-key`. |
| `SAKUR4_LLAMA_URL` | unset | Base URL for the inference server, when no `--backend` is given. |
| `SAKUR4_SNAPSHOT_DIR` | the temp directory | Where `session.snapshot` writes save files. |
| `SAKUR4_EMBED_URL` | unset | OpenAI-compatible embedding endpoint, for semantic Atlas entries. |
| `SAKUR4_EMBED_MODEL` | `nomic-embed-text` | Model name to request from that endpoint. |
| `SAKUR4_EMBED_API_KEY` | unset | Key for a configured embedding endpoint. |
| `SAKUR4_PROJECT_ROOT` | the working directory | Root used to name a project for the Repo Cortex index. |
| `SAKUR4_EVICTION_PROFILE` | chosen from the backend | `cache-first` · `window-first` · `balanced` — overrides the automatic choice. |
| `SAKUR4_SQLITE_VEC_PATH` | searched | Path to a `sqlite-vec` extension, for vector search without the fallback scan. |
| `SAKUR4_TRANSPORT` | `stdio` | Transport for `serve`, when `--transport` is not given. |

### The installer

| Variable | Default | Meaning |
|---|---|---|
| `SAKUR4_VERSION` | the latest release | Tag to install, e.g. `v0.1.0`. |
| `SAKUR4_BIN_DIR` | `~/.local/bin`, else `/usr/local/bin` when writable | Where the binary lands. |
| `SAKUR4_SKILL_DIR` | `~/.agents/skills` | Where the Agent Skill is placed. |
| `SAKUR4_FORCE` | unset | Replace an existing skill instead of leaving it alone. |
| `SAKUR4_REPO` | `sc4rfurry/Sakur4` | Which repository to install from. |

### Oh My Pi extension extras

| Variable | Default | Meaning |
|---|---|---|
| `SAKUR4_RETRIEVE` | `true` | Inject retrieved memory before each turn. |
| `SAKUR4_REPORT_USAGE` | `true` | Report provider usage automatically. |
| `SAKUR4_OWN_COMPACTION` | `true` | Take over compaction. |
| `SAKUR4_RECALL_BUDGET` | `1200` | Approximate token cap on injected memory. |
| `SAKUR4_PLUGIN_LOG` | unset | Append lifecycle diagnostics to this file. |

### Hermes ContextEngine

| Variable | Default | Meaning |
|---|---|---|
| `SAKUR4_URL` | `http://127.0.0.1:8765` | Where the daemon is listening. |
| `SAKUR4_THRESHOLD_PERCENT` | `0.75` | Fraction of the window at which compaction fires. |

> **A variable that is set and does nothing is worse than one that does not exist**, so this list is checked: `docs/verification/env-vars.mjs` **fails** if a name documented here appears nowhere in the code. It was written after finding several that were read by the code and named by no document — `SAKUR4_LLAMA_API_KEY` among them, which is the one anybody running a gated server needs first.

## The three backends

`--backend` / `SAKUR4_BACKEND` accepts `auto`, `embedded`, `none`, or an HTTP base URL. Everything is optional; the defaults work.

| Backend | What it is | Cache coherence |
|---|---|---|
| **`llama.cpp`** (a URL) | A real server. Probes `/slots`, `/props`, `/tokenize`, `/metrics`. | **Full** |
| **`embedded`** | Simulates a checkpoint ring in-process. **Default when nothing is listening.** | Full, simulated |
| **`none`** | Coherence disabled; degradation logged. | Reported as `full-re-prefill` |

`auto` probes and resolves to one of the above; `--backend` also accepts a URL on another machine.

```bash
llama-server -m model.gguf -c 65536 --slots -cms 256 -ctxcp 64
sakur4d --backend http://127.0.0.1:8080 doctor
```

`doctor` prints exactly which endpoints were detected and **what that means for compaction** — so a backend resolved as `embedded` explains a simulated verdict rather than leaving it mysterious.

**A real llama.cpp build may expose no checkpoint API at all** (save/erase returning 501, no checkpoint ring). Sakur4 then correctly reports `no checkpoint source detected`, and prefix reuse still works — a preserved prefix followed by new content is reused in full. The practical consequence is that the receipt is **pessimistic** on such a backend: it reports `full-re-prefill` where reuse is real but unverifiable.

## Eviction profiles

The profile is **chosen from what the backend can actually do**, not from a constant — a user should not have to know their server lacks checkpoints in order to get the right ratios.

| Profile | Trigger | Target | Prefix reserve | Cache alignment | Selected when |
|---|---|---|---|---|---|
| **`cache-first`** | 75% | 55% | 4096 | on | The backend exposes a checkpoint ring, so a preserved prefix is genuinely reusable. |
| **`window-first`** | 70% | 30% | 0 | off | There is no checkpoint source, so a large working set buys nothing and window room is the scarcer resource. |
| **`balanced`** | 75% | 45% | 2048 | on | Named explicitly, for a user who wants neither extreme. |

All three keep `4096` tokens of recent context verbatim. The setting is treated as an **upper bound** and shrunk to at most a quarter of the window, so a genuinely small window still has a middle to evict at — a 4,096-token window reserving 4,096 tokens of recent context would leave nothing evictable at all.

`SAKUR4_EVICTION_PROFILE` **overrides the probe** and is not second-guessed: `window-first` on a checkpoint-capable server is a legitimate thing to want, trading cache reuse for window room. The parser is case-insensitive and accepts short forms:

| Accepted | Profile |
|---|---|
| `cache-first`, `cache`, `coherent` | `cache-first` |
| `window-first`, `window`, `tight` | `window-first` |
| `balanced`, `default` | `balanced` |

**What the choice is worth.** Measured against a real llama.cpp build exposing no checkpoints, `cache-first` cost **29% more tokens per turn** while buying nothing. Switching to the capability-derived profile brought the overhead to **+1.3%** while keeping **42 points** of recall — the entire difference was the target ratio, since anchors and retrieval accounted for 0.3%.

## Common configurations

### Try it with no model at all

```bash
sakur4d demo --db :memory:
sakur4d --backend embedded doctor
```

No GPU, no model, no network. The embedded backend simulates a llama.cpp checkpoint ring in-process, so every code path runs.

### A local llama.cpp server with a checkpoint ring

```bash
llama-server -m model.gguf -c 65536 --slots -cms 256 -ctxcp 64

sakur4d --backend http://127.0.0.1:8080 --db ~/.sakur4/sakur4.db doctor
sakur4d --backend http://127.0.0.1:8080 --db ~/.sakur4/sakur4.db serve
```

`doctor` should report `checkpoint-aligned eviction boundaries available`. The profile then selects itself as `cache-first`.

### A gated server that needs a bearer token

```bash
export SAKUR4_LLAMA_API_KEY=your-token-here
export SAKUR4_LLAMA_URL=http://127.0.0.1:8080
sakur4d doctor
```

`SAKUR4_LLAMA_URL` is used when no `--backend` is given; the key is sent as a bearer token. This is the one anybody running a server behind a reverse proxy or started with `--api-key` needs first.

### A server with no checkpoint API

```bash
SAKUR4_EVICTION_PROFILE=window-first sakur4d serve
```

Shed to a small working set and keep only the recent tail and the anchors. This is the profile the capability probe would have chosen anyway — naming it makes the choice visible in `doctor`.

### One store, several sessions

```bash
sakur4d --db ~/.sakur4/shared.db serve --transport http --bind 127.0.0.1:8765
```

Then point each harness at `http://127.0.0.1:8765/`. Keep the bind on localhost: **there is no authentication on the transports**, and `--bind 0.0.0.0` exposes the entire Memory Fabric, including writes.

### Real semantic recall

```bash
export SAKUR4_EMBED_URL=http://127.0.0.1:8081/v1
export SAKUR4_EMBED_MODEL=nomic-embed-text
sakur4d doctor
```

With no embedding endpoint, the built-in embedder is `sakur4-hashing-v1` — deterministic hashing of word unigrams and character trigrams, local and dependency-free, but **lexical rather than semantic**. Configure an endpoint for genuine semantic recall. `SAKUR4_SQLITE_VEC_PATH` points at a `sqlite-vec` extension if you want vector search without the fallback exact-cosine scan.

### A small context window

```bash
sakur4d serve --context-window 8192
```

Worth setting explicitly whenever the detected window is wrong: an explicit value **wins** over the backend's reported one, and the daemon needs to know the real window to plan eviction against it.

### Encryption at rest

```bash
cargo build --release --features encryption
sakur4d gen-key > key.txt
```

Encryption is **off by default**, and the store is otherwise readable by anyone with file access — and it holds a verbatim transcript. Keys are 64 hex characters, a raw 256-bit key rather than a passphrase, so there is no derivation step to attack; a short key is refused rather than stretched. `gen-key` prints to stdout and nothing else, so the redirect above works, and **it never writes the key anywhere itself**. In a build without the feature it fails with `this build has no encryption support, so a key would be unusable` rather than handing you a key nothing can use.

## Checking what resolved

```bash
sakur4d doctor              # resolved backend, detected capabilities, active profile, store counts
sakur4d doctor --refresh    # re-probe rather than reporting cached capabilities
sakur4d --help              # every flag's `[env: NAME=]` annotation, and its default
```

`doctor` is the single answer to "did my configuration take effect": it shows the store it opened, the schema version, the vector backend, the tokenizer, the **embedder**, the **active eviction profile with its trigger and target**, and the context window it will plan against. If a value there does not match what you set, the flag or variable is the place to look.

---

<sub>[← Back to Home](Home) · [All pages](Home#where-to-go-next)</sub>
