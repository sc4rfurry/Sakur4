Install `sakur4d`, run it once by hand, and complete a first memory session from the terminal.

---

**On this page:** [Requirements](#requirements) · [Install](#install) · [Verifying the checksum](#verifying-the-checksum) · [First run](#first-run) · [A first session](#a-first-session) · [What to do next](#what-to-do-next) · [Install troubleshooting](#install-troubleshooting)

## Requirements

| Need | Detail |
|---|---|
| `sakur4d` | One self-contained binary with **no runtime dependencies**. |
| A model | **None.** No GPU, no model and no network are required — the embedded backend simulates a llama.cpp checkpoint ring in-process, so everything works anywhere. |
| Rust **1.94+** | Only to build from source. |
| Node.js **18+** | Only for the Oh My Pi extension and the Agent Skill. |
| A POSIX `sh` | Only for the install script, which is `#!/bin/sh`. On Windows use the release archive or build from source. |

## Install

### The one-liner

```bash
curl -fsSL https://raw.githubusercontent.com/sc4rfurry/Sakur4/master/install.sh | sh
```

It detects your platform, **verifies the checksum**, installs the binary, and places the Agent Skill in `~/.agents/skills/`. An existing skill is left alone unless `SAKUR4_FORCE=1`.

Three environment variables change what it does:

| Variable | Effect |
|---|---|
| `SAKUR4_VERSION` | Pin a release instead of resolving the latest — for example `SAKUR4_VERSION=v0.1.0`. |
| `SAKUR4_BIN_DIR` | Where the binary lands. Default order: `~/.local/bin` if it is already on `PATH`, then `/usr/local/bin` if writable, then `~/.local/bin` regardless. |
| `SAKUR4_SKILL_DIR` | Where the Agent Skill is copied. Defaults to `~/.agents/skills`. |

### The other three routes

| Method | Command | Notes |
|---|---|---|
| **Release binary** | Download from [Releases](https://github.com/sc4rfurry/Sakur4/releases) | Archives carry `sakur4d`, the Agent Skill and both integrations. The script installs the binary and the skill; the plugins are unpacked alongside, because each harness keeps plugins elsewhere. |
| **From a checkout** | `cargo install --path crates/sakur4d` | Builds and installs in one step — about **eight minutes** from cold. |
| **From source** | `cargo build --release` | Then copy `target/release/sakur4d` onto your `PATH`. |

Put the binary somewhere on `PATH`. `~/.cargo/bin` is where `cargo install` puts it and where **every integration looks first**. If it lives somewhere unusual, set `SAKUR4_BIN` and everything will find it.

Publishing the crates yourself? Publish `cargo publish -p sakur4-core` **first**. `cargo publish` resolves a path dependency through the registry, so the binary crate cannot be published until the core crate is on crates.io — the other order fails with `no matching package named 'sakur4-core' found`.

## Verifying the checksum

**The installer refuses to install an archive it cannot verify.** If `SHA256SUMS.txt` is unreachable, or does not list your platform's archive, it stops rather than continuing — an installer that silently skips verification teaches people to trust the output of a pipe.

What it does, in order:

1. Fetches `sakur4d-<version>-<target>.tar.gz` from the release.
2. Fetches `SHA256SUMS.txt` **separately**, and dies with `could not fetch SHA256SUMS.txt; refusing to install without verification` if that fails.
3. Looks your archive up in the file, and dies with `is not listed in SHA256SUMS.txt; refusing to install` if it is absent.
4. Computes the digest with `sha256sum`, or `shasum -a 256` on systems without coreutils, and compares. A mismatch prints both digests and says **do not use this download**.
5. Only then extracts and installs, and finally runs `sakur4d --version` to confirm the installed binary executes.

To check a download by hand, take `SHA256SUMS.txt` from the same release page and compare the line for your archive against:

```bash
sha256sum sakur4d-<version>-<target>.tar.gz      # GNU coreutils
shasum -a 256 sakur4d-<version>-<target>.tar.gz  # macOS and most BSDs
```

**What this does and does not prove.** Release archives carry SHA-256 checksums, which detect **corruption and not tampering**. There is no GPG signature and no build provenance attestation on the release artifacts.

## First run

Nothing here needs a configured harness. The guided walkthrough drives a session past its context budget, shows the eviction plan the engine chose, and prints the cache verdict for the resulting boundary:

```bash
sakur4d demo --db :memory:
```

`--db :memory:` keeps it away from your real store. It needs nothing but the binary.

**Nothing in the demo is a special path.** The receipt it prints is the same one `context.receipt` returns over MCP, and the plan is the same one `context.plan_eviction` returns.

Then look at what was detected on your machine:

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
  episodes         0
  symbolic facts   0
  atlas entries    0
  stale entries    0
  anchors          0
  open folds       0
  repo files       0
```

`doctor` prints exactly which cache endpoints were detected and **what that means for compaction**. Read the `coherence` line first: it decides whether you have cache-coherent compaction or should pin more aggressively. `--refresh` re-probes the backend instead of reporting cached capabilities.

## A first session

Five commands, in the order you would actually run them.

### 1 · `index` — build the code graph

```console
$ cd /path/to/your/project
$ sakur4d index .
indexed .
  117 scanned, 117 parsed, 0 unchanged, 0 removed, 0 unsupported · 2296 symbol(s), 17352 edge(s) · 2030 ms · [rust×49, javascript×33, config×32, python×2, typescript×1]
```

Re-running is **incremental**, so it is cheap. `--full` forces a full re-parse.

**Nothing re-indexes automatically** — there is no filesystem watcher. Run `index` after changing files; the repo map dates itself so you can see when it was built.

Then read the shape of the project instead of opening files in bulk:

```bash
sakur4d repo-map --budget 1500
sakur4d repo-map --budget 600 --names      # qualified names, which `symbol` and `impact` accept
```

**To query a symbol, get its real name first.** Qualified names carry their whole path, so they look nothing like a file path:

```console
$ sakur4d repo-map --budget 600 --names
=== REPOSITORY MAP (ranked by structural centrality) ===

crates/sakur4-core/src/engine.rs  (rank 0.01)
  crates::sakur4-core::src::engine::Engine
  crates::sakur4-core::src::engine::Engine::open
  crates::sakur4-core::src::engine::EngineConfig::from_toml_path
```

The shape is `path::Type::member`, so a method is qualified by the type it is implemented on — `Engine::open` and `Server::open` are different facts. Against your own project the names will be yours; `repo-map --names` is how you find them.

### 2 · `commit` — record a turn

```text
sakur4d commit <SESSION> <CONTENT>        # --role, --tool, --slot
```

```console
$ sakur4d commit auth-refactor "add rate limiting to the login endpoint" --role user
episode ep_01a0d4ed5f0d75dc8e622acedcebce5b (seq 1) · 10 tokens · symbolic: not a tool result
```

`commit` appends to an **append-only** store; nothing is ever rewritten, so anything you record can be recalled verbatim later — including after compaction has removed it from the agent's own window. Pass `--tool` on a tool result so the symbolic extractor can pick a parser, which turns a diff, a JSON body or an exit status into deterministic facts rather than prose.

Commit sometimes suggests a pin when it sees a constraint in a user turn:

```console
$ sakur4d commit auth-refactor "never force-push to main" --role user
episode ep_01a0d4ee4bc573959f9af0170c7c934b (seq 2) · 7 tokens · symbolic: not a tool result
  a constraint may have been stated (safety_constraint — rule explicit_never, confidence 0.70):
    never force-push to main
  pin it with `sakur4d pin <text> --kind safety_constraint` if it should survive every compaction
```

That is a **proposal, not an action**. Pin it only if it really is a standing rule.

### 3 · `status` — what the layer is holding

At the shell this is `doctor` (above). Over MCP it is the `sakur4.status` tool, which reports the same resolved backend, detected capabilities, tokenizer, embedder and per-store counts, plus `project_episodes`, `project_facts` and `project_atlas` scoped to the project the daemon was started for.

```bash
sakur4d doctor --refresh
sakur4d anchors --session auth-refactor
```

```console
$ sakur4d pin "rate limits must be configurable, not hardcoded" --kind task_contract --session auth-refactor
pinned anc_01a0d4ed5f3f72639e9d8472127c9ff0 as task_contract — this entry is now exempt from every eviction tier

$ sakur4d anchors --session auth-refactor
[task_contract] rate limits must be configurable, not hardcoded  (18 tokens, pinned by user)

1 anchor(s), 18 tokens pinned in every prompt
```

The three kinds are `safety_constraint`, `user_correction` and `task_contract`. **Pinned content is rendered verbatim into every prompt and is exempt from every eviction tier** — so each pin costs tokens on every turn. Pin rules and corrections, not status updates.

### 4 · `recall` — ask the store instead of guessing

```bash
sakur4d recall "rate limiting" --k 3
```

```console
$ sakur4d recall "rate limiting" --k 3
 1. [episode] score 0.600  (bm25)
      add rate limiting to the login endpoint
```

`--k` defaults to `8`. `--session` restricts to one session and `--include-folded` includes folded subtask traces.

Results marked `STALE` are summaries whose source has changed. They come back with the source's **current** value attached — trust that value, not the summary above it. This is the single most important behaviour to respect: a stale summary is confidently wrong, and acting on it is how an agent does the thing it was told not to do.

### 5 · When a turn felt slow

```bash
sakur4d receipt <SESSION>          # where the budget went, and the cache verdict
sakur4d plan <SESSION>             # what an eviction would do, without applying it
```

`plan` plans by **default**. Nothing changes until you pass `--apply`.

> **Await each answer before sending the next call.** MCP permits a server to process a queued batch of requests in an arbitrary order, and Sakur4's tools are stateful. Sending one call and awaiting its reply — which every harness tested here does — is correct.

## What to do next

| If you want to… | Read |
|---|---|
| Wire Sakur4 into your harness | **[Harnesses](Harnesses)** — MCP, Oh My Pi, Hermes, Agent Skill, reverse proxy |
| See every tool, argument and trigger | **[Tool Reference](Tool-Reference)** |
| Set flags and environment variables | **[Configuration](Configuration)** |
| Look up a subcommand | **[CLI Reference](CLI-Reference)** |
| Know what does **not** work yet | **[Limitations](Limitations)** |

## Install troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `could not determine the latest release; set SAKUR4_VERSION` | The GitHub releases API was unreachable, or rate-limited. | Set the tag explicitly: `SAKUR4_VERSION=v0.1.0`. |
| `could not fetch SHA256SUMS.txt; refusing to install without verification` | The checksum file could not be downloaded. | Fix network or proxy access, or download the archive and checksums from the release page and verify by hand. |
| `<archive> is not listed in SHA256SUMS.txt; refusing to install` | The release does not carry an archive for your target. | Check the target triple the script printed against the release assets, and build from source if it is genuinely missing. |
| `checksum mismatch for <archive>` | The download is corrupt — or something rewrote it in transit. | **Do not use the download.** Re-download; if it repeats, report it. |
| `sha256sum or shasum is required; this installer verifies before it installs` | Neither coreutils nor `shasum` is installed. | Install one of them — the installer will not skip verification. |
| `cannot create <dir>; set SAKUR4_BIN_DIR` | The chosen install directory is not writable. | `SAKUR4_BIN_DIR=~/.local/bin` (or any writable directory on `PATH`). |
| `could not be written to …; set SAKUR4_SKILL_DIR` | The skill destination is not writable. | Point `SAKUR4_SKILL_DIR` somewhere you own. |
| `sakur4d` installs but the shell cannot find it | The install directory is not on `PATH`. | The script prints the exact `export PATH=…` line when the directory it chose is not already on `PATH`; `SAKUR4_BIN_DIR` picks a different one. |
| A toolchain older than 1.94 cannot build the workspace | The MSRV is **1.94**. | Upgrade Rust, or take a release binary instead of building. |
| `no matching package named 'sakur4-core' found` when publishing | `cargo publish` resolves a path dependency through the registry. | Publish `-p sakur4-core` **first**, then the binary crate. |

---

<sub>[← Back to Home](Home) · [All pages](Home#where-to-go-next)</sub>
