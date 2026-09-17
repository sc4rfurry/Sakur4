---
name: sakur4
description: Persistent memory and context management for long coding sessions, backed by a local Sakur4 daemon. Records turns and tool results, pins constraints so they survive compaction, retrieves prior work by meaning, reports where the context budget went, and measures whether compaction broke the provider's prompt cache. Use when a session runs long, when you need to recall something from earlier, when the user states a rule or correction that must not be forgotten, or when a turn felt slow or expensive.
license: Apache-2.0
compatibility: Needs Node.js 18+ and a sakur4d binary (cargo install sakur4d, or a release download). Works without a GPU, model, or network.
---

# Sakur4

Sakur4 is a memory and context layer that sits beside your session. It keeps a
verbatim record of what happened, lets you find it again by meaning rather than by
grepping the transcript, and reports where the context budget actually went.

Everything below works through one CLI, so it works the same in any harness.

## Setup

Locate the daemon. If `sakur4d` is on `PATH`, nothing more is needed; otherwise set
`SAKUR4_BIN` or pass `--bin`.

```bash
# Check it works and see what it detected.
node scripts/sakur4.mjs doctor

# Or point at a specific binary.
node scripts/sakur4.mjs doctor --bin /path/to/sakur4d
```

`doctor` prints the resolved inference backend, the detected cache capabilities, the
tokenizer in use, and counts for each memory store. Read it once at the start of a
long session — it tells you whether cache-coherent compaction is available or
whether you should pin more aggressively.

## The one thing to do every turn

```bash
node scripts/sakur4.mjs commit --role user --content "refactor the auth module"
node scripts/sakur4.mjs commit --role tool --tool read_file --file /tmp/result.json
```

`commit` appends to an append-only store. Nothing is ever rewritten, so anything you
record can be recalled verbatim later — including after compaction has removed it
from your own context window.

Structured tool output (JSON, CSV, HTTP headers, diffs, exit codes) is additionally
parsed into deterministic facts. If you pipe a file with `--file`, the structured
parser gets the real bytes.

**Commit the user's turns verbatim.** Do not paraphrase what they asked for. Their
words are the source everything else derives from, and a paraphrase loses exactly the
part that matters.

## When the user states a rule, corrects you, or sets a requirement

Pin it. Unpinned requirements get compacted away; pinned ones cannot be.

```bash
node scripts/sakur4.mjs pin --kind safety_constraint --content "never force-push to main"
node scripts/sakur4.mjs pin --kind user_correction --content "the function is validateUser(id), not checkUser(email)"
node scripts/sakur4.mjs pin --kind task_contract --content "keep the public API of src/auth.rs backward compatible"
```

The three kinds are `safety_constraint`, `user_correction`, and `task_contract`.
Pinned content is rendered verbatim into every prompt and is exempt from every
eviction tier. Each pin costs tokens on every turn, so pin rules and corrections —
not status updates.

`commit` will sometimes print a suggested pin when it detects a constraint in a user
turn. That is a proposal, not an action; pin it only if it really is a standing rule.

## When you cannot remember something

Do not guess. Ask the store.

```bash
node scripts/sakur4.mjs recall --query "how did we decide to handle retries" --k 5
node scripts/sakur4.mjs recall --query "TokenValidator" --json
```

Results marked `STALE` are summaries whose source has since changed. They come with
the source's **current** value attached — trust that value, not the summary above
it. This is the single most important behaviour to respect: a stale summary is
confidently wrong, and acting on it is how an agent does the thing it was told not
to do.

## Before starting work that will take many steps

Wrap it in a fold. The intermediate steps leave your context, and the subtask
collapses to a one-line result when you close it.

```bash
node scripts/sakur4.mjs fold --description "trace the validate() call path" --goal "find every caller"
# ... do the work, committing as you go ...
node scripts/sakur4.mjs unfold --fold-id fold_01a0... --summary "validate() is called from login() only"
```

Use a fold when you expect to read many files, run many greps, or explore an
approach you may abandon. The full trace stays retrievable with `recall-fold`.

## When a turn felt slow or expensive

```bash
node scripts/sakur4.mjs receipt
```

This prints where the token budget went, category by category, and the cache
verdict: whether the prompt reused the provider's cached prefix, or had to be
reprocessed from scratch. On a hosted provider it also reports whether a
compaction **broke** the cache — which is a bill, not just a delay.

## Reporting what the provider charged

If your harness gives you token usage from the model response, report it once per
turn. Sakur4 then tracks prompt-cache behaviour across the session.

```bash
node scripts/sakur4.mjs usage --prompt-tokens 6200 --cache-read-tokens 300
```

Field names differ by provider; normalise to these:

| Provider | Field to read |
|---|---|
| OpenAI | `prompt_tokens_details.cached_tokens` |
| Anthropic | `cache_read_input_tokens` |
| DeepSeek | `prompt_cache_hit_tokens` |
| Gemini | `cachedContentTokenCount` |
| Groq / others | often absent — omit the flag rather than sending zero |

Omitting the cache flag is not the same as sending `0`. Zero asserts a cache miss;
omitting says the provider did not report one, and Sakur4 says so rather than
blaming a cache it cannot see.

## Keeping memory useful

```bash
# Promote substantial turns into searchable summaries, regenerate stale ones,
# embed anything missing a vector, archive what has gone cold.
node scripts/sakur4.mjs dream

# Find out which stored summaries no longer match their source.
node scripts/sakur4.mjs staleness
```

Run `dream` when you are between tasks and nothing is generating. It refuses to run
while a session is active, so it is safe to call any time.

## Working with a codebase

```bash
node scripts/sakur4.mjs index --root .
node scripts/sakur4.mjs map --budget 1500
node scripts/sakur4.mjs map --budget 800 --names      # the names symbol/impact accept
node scripts/sakur4.mjs symbol --name "src::auth::validate"
node scripts/sakur4.mjs impact --name "src::auth::validate"
```

**Use `--names` to find a name before looking one up.** Ordinary `map` output shows each
symbol's *signature*, which is the more useful rendering per token — but `symbol` and `impact`
take *qualified names*, and a signature is not one. So `map` alone cannot tell you what to pass
them, and the two example names above (`src::auth::validate`) are placeholders that will not
exist in your repository. `map --names` prints the real ones, and they look like this:

```text
crates/sakur4-core/src/engine.rs  (rank 0.02)
  crates::sakur4-core::src::engine::EngineConfig
  crates::sakur4-core::src::engine::Engine::open
  crates::sakur4-core::src::engine::EngineConfig::from_toml_path
```

Copy one of those verbatim. The shape is `path::Type::member`, so a method is qualified by the
type it is implemented on — `Engine::open` and `Server::open` are different facts.

- `index` parses the repository and builds a call and import graph. Re-running it
  is incremental, so it is cheap.
- `map` prints a structural outline ranked by how load-bearing each symbol is,
  fitted to a token budget. Call it before opening files in bulk. Add `--names` to
  get the qualified names that `symbol` and `impact` accept; the default output shows
  signatures, which is more useful per token but is not a name you can pass on.
- `symbol` returns a symbol's **current** signature from the parser — this cannot be
  stale, unlike anything you remember.
- `impact` lists every call site that depends on a symbol, transitively. Run it
  before changing a signature.

## Environment

| Variable | Meaning |
|---|---|
| `SAKUR4_BIN` | Path to the `sakur4d` binary, if it is not on `PATH`. |
| `SAKUR4_DB` | Memory store path. Default: `~/.sakur4/sakur4.db`. |
| `SAKUR4_SESSION` | Session id used when `--session` is not passed. Default: the current directory name. |
| `SAKUR4_BACKEND` | `auto` (default), `embedded`, `none`, or a llama.cpp base URL. |

Every command accepts `--session`, `--bin`, and `--db`, so environment variables are
conveniences rather than requirements.

## A worked example

```bash
# Start of a long task.
node scripts/sakur4.mjs commit --role user --content "add rate limiting to the login endpoint"
node scripts/sakur4.mjs pin --kind task_contract --content "rate limits must be configurable, not hardcoded"
node scripts/sakur4.mjs index --root .
node scripts/sakur4.mjs impact --name "src::auth::login"

# Explore inside a fold.
node scripts/sakur4.mjs fold --description "find the middleware chain" --goal "identify where to add the limiter"
node scripts/sakur4.mjs recall --query "existing middleware registration"
node scripts/sakur4.mjs unfold --fold-id <id> --summary "middleware is registered in src/app.rs:42 via AppBuilder"

# Later, when something seems familiar but you are not sure.
node scripts/sakur4.mjs recall --query "the app builder" --k 3
node scripts/sakur4.mjs symbol --name "src::app::AppBuilder::register"

# If a turn felt slow.
node scripts/sakur4.mjs receipt
```

## What this skill deliberately does not do

It does not replace compaction or decide what leaves your context window — that is
the harness's job, and Sakur4's eviction engine only ever advises. It does not call
a model. And it does not claim to know something it did not store: if `recall`
returns nothing, the honest next step is to read the file, not to reconstruct the
answer from memory.
