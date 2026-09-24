Every `sakur4d` subcommand — its usage line, its flags, and a realistic example.

---

**On this page:** [Conventions](#conventions) · [All subcommands](#all-subcommands) · [Running the daemon](#running-the-daemon) · [Code intelligence](#code-intelligence) · [Memory](#memory) · [Context and sessions](#context-and-sessions) · [Maintenance](#maintenance) · [Learning](#learning) · [Not on the command line](#not-on-the-command-line)

## Conventions

```text
sakur4d [OPTIONS] <COMMAND> [ARGS]
```

Global options come **before** the subcommand: `--db`, `--backend`, `--project-root`, `--embed-url`, `--embed-model`, `--context-window`, `-v`. Their defaults and environment variables are in **[Configuration](Configuration)**.

```bash
sakur4d --db ~/.sakur4/sakur4.db --backend http://127.0.0.1:8080 doctor
```

Examples below usually omit them, which means "the defaults apply" — `sakur4.db` in the working directory, and `auto` for the backend.

## All subcommands

| Group | Commands |
|---|---|
| Running the daemon | `serve` · `proxy` · `config` · `doctor` · `gen-key` |
| Code intelligence | `index` · `repo-map` · `symbol` · `impact` |
| Memory | `commit` · `pin` · `anchors` · `recall` |
| Context and sessions | `plan` · `receipt` · `snapshot` · `restore` |
| Maintenance | `dream` · `staleness` |
| Learning | `demo` |

## Running the daemon

### `serve`

```text
sakur4d serve [OPTIONS]
```

Run the MCP gateway. **`stdio` is the default** because every MCP client can spawn a child process; `http` is for shared stores, remote harnesses, and concurrent sessions.

```bash
sakur4d serve                                            # stdio, the default
sakur4d serve --transport http --bind 127.0.0.1:8765     # shared
sakur4d serve --banner                                   # startup diagnostics on stderr
```

| Flag | Default | Notes |
|---|---|---|
| `--transport` | `stdio` | `stdio`, `http`, `http://host:port`, or a bare `host:port`. |
| `--bind` | `127.0.0.1:8765` | Address to bind when serving over HTTP. Defaults to localhost, per NFR-11. |
| `--no-dream` | off | Disable the Idle Consolidator. |
| `--quiet-secs` | `90` | Seconds of quiet before consolidation may run. |
| `--banner` | off | Print a startup banner to **stderr**. |

The Idle Consolidator is **on by default** — memory maintenance should not require opting in — and it refuses to run while any tracked slot is generating. `--banner` is off because over stdio the client owns this process, and a harness that captures stderr collects a banner on every session.

### `proxy`

```text
sakur4d proxy [OPTIONS]
```

Run as an OpenAI-compatible reverse proxy in front of the inference server (FR-18). For a harness that has neither MCP nor a plugin system: point it at this address instead of at `llama-server` and nothing else changes.

```bash
sakur4d proxy --bind 127.0.0.1:8090 --upstream http://127.0.0.1:8080
# harness base URL: http://127.0.0.1:8090/v1

sakur4d proxy --observe-only --upstream http://127.0.0.1:8080 --session my-session
```

| Flag | Default | Notes |
|---|---|---|
| `--bind` | `127.0.0.1:8090` | Address to listen on. Point the harness here. |
| `--upstream` | `http://127.0.0.1:8080` | The real inference server. |
| `--session` | — | Session id reported to the Memory Fabric. |
| `--observe-only` | off | Forward requests unchanged, **recording only**. |

Requests are forwarded untouched, every unrecognised route included; a transcript that exceeds the window is trimmed on the way through with a marker left where the removed turns were. `--observe-only` is how to measure on real traffic before letting it act.

> **Do not run the proxy and the OMP extension at the same time** — a turn gets managed twice and OMP hangs before sending its first request.

### `config`

```text
sakur4d config [OPTIONS] [HARNESS]
```

Print ready-to-paste MCP configuration for a harness, with **this binary's absolute path and store baked in** — so there is no placeholder to forget.

```bash
sakur4d config hermes          # ~/.hermes/config.yaml     (the default)
sakur4d config claude          # claude_desktop_config.json
sakur4d config claude-code     # one-line CLI registration
sakur4d config generic-http    # anything that connects to a URL
sakur4d config generic-stdio   # anything that spawns a child process
sakur4d config claude --binary /usr/local/bin/sakur4d
```

`[HARNESS]` defaults to `hermes`. `--db` is resolved to an **absolute** path before printing, because a harness picks the working directory and a relative store path would resolve somewhere unexpected.

### `doctor`

```text
sakur4d doctor [OPTIONS]
```

Print resolved configuration and component status.

```console
$ sakur4d --backend embedded doctor
Sakur4 0.1.0
  store            sakur4.db
  schema           v4 · wal · 282624 bytes
  vector backend   built-in exact cosine scan
  lexical index    FTS5 (BM25)

inference backend
  requested        embedded
  resolved         embedded (sakur4://embedded)
  note             using the embedded backend (no external server required)
  capabilities     slots+save+restore+checkpoint-ring+tokenize+metrics
  coherence        checkpoint-aligned eviction boundaries available

context management
  tokenizer        backend-exact
  embedder         sakur4-hashing-v1 (dim 384, deterministic hashing of word unigrams + character trigrams; local and dependency-free, but lexical rather than semantic — configure a local embedding endpoint for real semantic recall)
  recall paths     bm25, vector(sakur4-hashing-v1, deterministic), graph
  eviction         cache-first · trigger 75% · target 55% · keep 4096 recent
  context window   32768 tokens

memory fabric
  episodes         2
  symbolic facts   1
  atlas entries    0
  stale entries    0
  anchors          1
  open folds       0
  repo files       0
```

`--refresh` re-probes the backend rather than reporting cached capabilities. **This is the command to run when a configuration does not seem to have taken effect** — it reports what actually resolved, not what you asked for.

### `gen-key`

```text
sakur4d gen-key [OPTIONS]
```

Generate a key for an encrypted store, and print it.

```bash
sakur4d gen-key > key.txt
```

Keys are **64 hex characters** — a raw 256-bit key rather than a passphrase, so there is no derivation step to attack, and a short key is refused rather than stretched. It prints to stdout and nothing else, and **never writes the key anywhere itself**. In a build without encryption support it fails rather than handing you a key nothing can use:

```console
$ sakur4d gen-key
Error: this build has no encryption support, so a key would be unusable.
Rebuild with `cargo build --features encryption` (needs OpenSSL development files), or install a
release binary, which includes it.
```

## Code intelligence

### `index`

```text
sakur4d index [OPTIONS] [PATH]
```

Build or refresh the Repo Cortex index.

```console
$ sakur4d index .
indexed .
  117 scanned, 117 parsed, 0 unchanged, 0 removed, 0 unsupported · 2296 symbol(s), 17352 edge(s) · 2030 ms · [rust×49, javascript×33, config×32, python×2, typescript×1]
```

`[PATH]` defaults to `--project-root` or the working directory. **Re-running is incremental**, so it is cheap; `--full` forces a full re-parse.

**Nothing re-indexes automatically** — there is no filesystem watcher, so run `index` after changing files. The repo map dates itself so you can see when it was built.

### `repo-map`

```text
sakur4d repo-map [OPTIONS]
```

Print a token-budgeted structural map of the repository, ranked by structural centrality.

```console
$ sakur4d repo-map --budget 600 --names
=== REPOSITORY MAP (ranked by structural centrality) ===

crates/sakur4-core/src/engine.rs  (rank 0.01)
  crates::sakur4-core::src::engine::Engine
  crates::sakur4-core::src::engine::Engine::open
  crates::sakur4-core::src::engine::EngineConfig::from_toml_path
```

| Flag | Default | Notes |
|---|---|---|
| `--budget` | `2000` | Token budget for the map. |
| `--focus` | — | Boost symbols reachable from these paths. |
| `--names` | off | Print **qualified names** instead of signatures, for use with `symbol` and `impact`. |

**Use `--names` to find a name before looking one up.** Ordinary output shows each symbol's *signature*, which is the more useful rendering per token — but `symbol` and `impact` take *qualified names*, and a signature is not one. So `map` alone cannot tell you what to pass them.

### `symbol`

```text
sakur4d symbol [OPTIONS] <QUALIFIED_NAME>
```

Look up a symbol's **current** deterministic signature.

```console
$ sakur4d symbol "crates::sakur4-core::src::engine::Engine::open"
method pub async fn open(config: EngineConfig) -> Result<Self> { (crates/sakur4-core/src/engine.rs:166)
  ast_hash  1e6e922d18eb338f
  source    tree_sitter
```

This is parser output, so it **cannot be stale** — unlike anything you remember. The shape is `path::Type::member`, so a method is qualified by the type it is implemented on.

**A name that does not exist is an error rather than an empty result**, deliberately — an empty answer would read as "nothing depends on this", which is a much worse thing to be told wrongly. It exits non-zero:

```console
$ sakur4d symbol src::auth::nope
Error: symbol src::auth::nope is not in the Symbolic Ledger; run `sakur4d index` first or check the
       qualified name
```

### `impact`

```text
sakur4d impact [OPTIONS] <SYMBOL>
```

Print the blast radius of changing a symbol, transitively.

```console
$ sakur4d impact "crates::sakur4-core::src::engine::Engine::open"
impact of changing crates::sakur4-core::src::engine::Engine::open (pub async fn open(config: EngineConfig) -> Result<Self> {)
  defined at crates/sakur4-core/src/engine.rs:166
  39 affected site(s):
    depth 1 via calls — crates::sakur4-core::src::store::db::Db::open (crates/sakur4-core/src/store/db.rs:57)
    depth 1 via calls — crates::sakur4-core::src::evict::EvictionEngine::plan (crates/sakur4-core/src/evict.rs:527)
```

`--depth` defaults to `4`. Run it **before changing a signature**, so the edit does not break callers you have not read. Each site carries `depth`, how it reaches the symbol, and whether it has changed since it last saw it.

```console
$ sakur4d impact src::auth::validate
Error: not found: symbol src::auth::validate is not in the Symbolic Ledger;
       index the project first (`sakur4d index`) or check the qualified name
```

## Memory

### `commit`

```text
sakur4d commit [OPTIONS] <SESSION> <CONTENT>
```

Append a turn to a session's Episodic Stream.

```console
$ sakur4d commit auth-refactor "add rate limiting to the login endpoint" --role user
episode ep_01a0d4ed5f0d75dc8e622acedcebce5b (seq 1) · 10 tokens · symbolic: not a tool result
```

A tool result names its tool, so the extractor can choose a parser:

```bash
sakur4d commit auth-refactor '{"status":"ok","file":"src/auth.rs"}' --role tool --tool read_file
```

| Flag | Default | Notes |
|---|---|---|
| `--role` | `user` | The turn's role. |
| `--tool` | — | Name of the tool that produced this content, when it is a tool result. |
| `--slot` | `0` | Slot the turn belongs to. |

**Pass `--tool`.** The symbolic extractor uses it to choose a parser, so a diff, a JSON body or a command's exit status becomes deterministic facts rather than being stored as unstructured prose. Without it, a `git diff` result is just text and nothing downstream can anchor to it.

When the content looks like a constraint, `commit` prints a **suggestion**:

```console
$ sakur4d commit auth-refactor "never force-push to main" --role user
episode ep_01a0d4ee4bc573959f9af0170c7c934b (seq 2) · 7 tokens · symbolic: not a tool result
  a constraint may have been stated (safety_constraint — rule explicit_never, confidence 0.70):
    never force-push to main
  pin it with `sakur4d pin <text> --kind safety_constraint` if it should survive every compaction
```

### `pin`

```text
sakur4d pin [OPTIONS] <CONTENT>
```

Pin a constraint into the Anchor Set.

```console
$ sakur4d pin "rate limits must be configurable, not hardcoded" --kind task_contract --session auth-refactor
pinned anc_01a0d4ed5f3f72639e9d8472127c9ff0 as task_contract — this entry is now exempt from every eviction tier
```

`--kind` defaults to `task_contract`; the three kinds are `safety_constraint`, `user_correction` and `task_contract`. `--session` scopes the pin.

Pinned content is rendered **verbatim into every prompt** and is exempt from every eviction tier — so each pin costs tokens on every turn. Pin rules and corrections, not status updates.

### `anchors`

```text
sakur4d anchors [OPTIONS]
```

Show the Anchor Set.

```console
$ sakur4d anchors --session auth-refactor
[task_contract] rate limits must be configurable, not hardcoded  (18 tokens, pinned by user)

1 anchor(s), 18 tokens pinned in every prompt
```

`--session` restricts the listing to one session. The same content is available over MCP at `sakur4://anchors/{project}`.

### `recall`

```text
sakur4d recall [OPTIONS] <QUERY>
```

Query the Memory Fabric — lexical, dense and graph retrieval merged and reranked.

```console
$ sakur4d recall "rate limiting" --k 3
 1. [episode] score 0.600  (bm25)
      add rate limiting to the login endpoint
```

| Flag | Default | Notes |
|---|---|---|
| `--k` | `8` | How many hits to return. |
| `--session` | — | Restrict to one session. |
| `--include-folded` | off | Include folded subtask traces. |

Results marked `STALE` are summaries whose source has changed. They come back carrying the source's **current** value — trust that, not the summary above it.

## Context and sessions

### `plan`

```text
sakur4d plan [OPTIONS] <SESSION>
```

Show or apply an eviction plan for a session.

```console
$ sakur4d plan auth-refactor
pressure: Relaxed
budget 32768 · trigger 24576 · target 18022 · live 32 (anchors 42, fixed 90)
0 episode(s), 0 tokens reclaimed (164 → 164 of 18022 target). cache alignment not evaluated
  note: context is at 164 tokens, below the 24576-token trigger; nothing to do
```

`--slot` defaults to `0`. **Plans by default — nothing changes until you pass `--apply`.**

```bash
sakur4d plan auth-refactor            # look
sakur4d plan auth-refactor --apply    # commit to it
```

### `receipt`

```text
sakur4d receipt [OPTIONS] <SESSION>
```

Print the Context Ledger Receipt: where the token budget went, and the cache verdict.

```console
$ sakur4d receipt auth-refactor
no receipts recorded for session auth-refactor

turns=0 reuse=0/0 (0%) partial=0 warm=0 full-reprefill=0 cold=0 · prompt-eval avg=0ms p95=0ms over 0 sample(s) · insufficient data for G1 (needs ≥5 compaction events)
```

| Flag | Default | Notes |
|---|---|---|
| `--history` | off | Print the whole history rather than only the latest. |
| `--limit` | `20` | How many receipts to include with `--history`. |

If it says `full-re-prefill`, the plan could not align to a checkpoint and **the reason is printed with it**. If it says `PREFIX-BROKEN`, a rewrite invalidated the provider's cache and you were billed for it.

### `snapshot`

```text
sakur4d snapshot [OPTIONS]
```

Force a KV-cache save for a slot.

```console
$ sakur4d snapshot --session auth-refactor
snapshot snap_01a0d4ed7536748ba408d25df461cba6 (141 bytes, 0 ms)
  file: <SAKUR4_SNAPSHOT_DIR>/slot0-01a0d4ed-7536-748b-a408-d25c52e9d640.json
  restore with: sakur4d restore --slot 0 --session auth-refactor <path>
```

`--session` defaults to `default`; `--slot` defaults to `0`. Save files go to the directory named by `SAKUR4_SNAPSHOT_DIR`, defaulting to the temporary directory, and are pruned by count.

> **Snapshots are as sensitive as the store.** A slot-save file is 60–500 MB of model state representing everything the session has seen, and nothing encrypts them.

### `restore`

```text
sakur4d restore [OPTIONS] <PATH>
```

Warm-restore a slot from a save file returned by `snapshot`.

```bash
sakur4d restore --slot 0 --session auth-refactor <path-from-snapshot>
```

`--session` defaults to `default`; `--slot` defaults to `0`. This is what turns a 60–120 second cold prefill into a sub-second restore.

## Maintenance

### `dream`

```text
sakur4d dream [OPTIONS]
```

Run one Idle Consolidator pass.

```console
$ sakur4d dream
dream cycle: promoted 0, regenerated 0, re-embedded 0, archived 0 · 2 ms
```

One pass promotes substantial turns into the Semantic Atlas, regenerates summaries whose source changed, embeds anything missing a vector, and archives long-cold episodes. It **refuses to run while any tracked slot is generating**, so it is safe to call at any time — but it is best run between tasks, when nothing is generating.

### `staleness`

```text
sakur4d staleness [OPTIONS]
```

Report recorded staleness between the Semantic Atlas and its anchors.

```console
$ sakur4d staleness
0 of 0 atlas entries are stale (0%), 0 with a deleted anchor
```

Use it to decide what to re-read rather than trusting a summary written a while ago. Staleness is computed **at read time**, by comparing an anchor's stored hash against its current one — so there is no window in which a stale interpretation looks fresh.

## Learning

### `demo`

```text
sakur4d demo [OPTIONS]
```

Guided end-to-end walkthrough against the embedded backend: dual-track write, staleness detection, a real compaction, the cache verdict, and round-trip integrity. Needs nothing but the binary.

```bash
sakur4d demo --db :memory:
sakur4d demo --repo . --db :memory:      # include the Repo Cortex portion
```

`--db :memory:` keeps it away from your real store. **Nothing in the demo is a special path** — the receipt it prints is the one `context.receipt` returns, and the plan is the one `context.plan_eviction` returns.

## Not on the command line

**`fold`, `unfold` and `recall_fold` exist as MCP tools only.** They are called by an agent mid-task, not by a person at a shell, and the Agent Skill's CLI exposes them through the protocol for that reason.

Everything else on this page has a tool equivalent with the same arguments — see **[Tool Reference](Tool-Reference)** for the mapping and for when an agent should call each one.

---

<sub>[← Back to Home](Home) · [All pages](Home#where-to-go-next)</sub>
