#!/usr/bin/env node
/**
 * sakur4 — a portable CLI over the Sakur4 daemon, for use from an agent skill.
 *
 * # Why this exists instead of calling `sakur4d` directly
 *
 * A skill has to work in whatever harness loads it, on whatever platform the user
 * runs. Three things make calling `sakur4d` directly awkward:
 *
 * 1. **Locating the binary.** It may be on `PATH`, installed by `cargo install`,
 *    sitting in a release download, or built from source. This resolves all of
 *    those, and says what it found rather than failing with "not found".
 * 2. **A stable default store.** Skills should not require the user to configure a
 *    path before the first call, but the default must not be a stray file in the
 *    working directory either.
 * 3. **A stable interface.** `sakur4d`'s CLI is for humans and may grow flags; this
 *    wrapper is the contract the skill text is written against.
 *
 * # Design constraints
 *
 * * **No dependencies.** Only `node:` builtins. A skill that needs `npm install`
 *   before it works is a skill that fails on first use.
 * * **No shell.** Every invocation goes through `spawnSync` with an argument array,
 *   so nothing is interpolated and a content argument containing quotes, newlines
 *   or backticks is passed through intact. This matters more than usual here: the
 *   primary use is committing user text verbatim.
 * * **Cross-platform.** Windows is a first-class target; the daemon is developed
 *   there. No `sh -c`, no path concatenation by hand.
 */

import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, statSync } from "node:fs";
import { homedir, platform } from "node:os";
import { dirname, isAbsolute, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const IS_WINDOWS = platform() === "win32";
const EXE = IS_WINDOWS ? "sakur4d.exe" : "sakur4d";

// ---------------------------------------------------------------------------
// Argument parsing
// ---------------------------------------------------------------------------

/**
 * Parse `--flag value`, `--flag=value`, `--bool` and positional arguments.
 *
 * Repeated flags collect into an array, because `pin` and `index` accept several.
 * Unknown flags are rejected rather than ignored: a typo that silently does nothing
 * is worse than an error, especially when the point of the call is to record
 * something that must not be lost.
 */
function parseArgs(argv, spec) {
  const out = { _: [] };
  const alias = spec.alias ?? {};
  for (let i = 0; i < argv.length; i++) {
    const token = argv[i];
    if (!token.startsWith("--")) {
      out._.push(token);
      continue;
    }
    let name = token.slice(2);
    let value;
    const eq = name.indexOf("=");
    if (eq >= 0) {
      value = name.slice(eq + 1);
      name = name.slice(0, eq);
    }
    name = alias[name] ?? name;
    const kind = spec.flags[name];
    if (!kind) {
      fail(`unknown option --${name}`, spec.usage);
    }
    if (kind === "bool") {
      out[name] = true;
      continue;
    }
    if (value === undefined) {
      value = argv[++i];
      if (value === undefined) {
        fail(`--${name} needs a value`, spec.usage);
      }
    }
    if (kind === "list") {
      (out[name] ??= []).push(value);
    } else {
      out[name] = value;
    }
  }
  return out;
}

function fail(message, usage) {
  process.stderr.write(`sakur4: ${message}\n`);
  if (usage) process.stderr.write(`\n${usage}\n`);
  process.exit(2);
}

// ---------------------------------------------------------------------------
// Locating the daemon
// ---------------------------------------------------------------------------

/**
 * Find `sakur4d`, in the order a user would expect.
 *
 * Returns `{ command, how }` or null. The `how` string is reported by `doctor`, so
 * "which binary am I actually talking to" is never a mystery — a stale build on
 * `PATH` shadowing a fresh one is otherwise very hard to notice.
 */
function findDaemon(explicit) {
  if (explicit) {
    const resolved = resolve(explicit);
    if (!existsSync(resolved)) {
      fail(`--bin points at ${resolved}, which does not exist`);
    }
    return { command: resolved, how: "--bin" };
  }

  const envBin = process.env.SAKUR4_BIN;
  if (envBin) {
    const resolved = resolve(envBin);
    if (!existsSync(resolved)) {
      fail(`SAKUR4_BIN points at ${resolved}, which does not exist`);
    }
    return { command: resolved, how: "SAKUR4_BIN" };
  }

  // Next to this script: the layout of a source checkout or a release archive,
  // where `sakur4d` and the skill ship together.
  const here = dirname(fileURLToPath(import.meta.url));
  const candidates = [
    join(here, "..", "..", "bin", EXE),
    join(here, "..", "..", "..", "target", "release", EXE),
    join(here, "..", "..", "..", "target", "debug", EXE),
    join(here, EXE),
  ];
  for (const candidate of candidates) {
    if (existsSync(candidate)) {
      return { command: resolve(candidate), how: "beside the skill" };
    }
  }

  // Finally, PATH.
  const probe = spawnSync(EXE, ["--version"], { encoding: "utf8" });
  if (!probe.error && probe.status === 0) {
    return { command: EXE, how: "PATH" };
  }

  return null;
}

function requireDaemon(opts) {
  const found = findDaemon(opts.bin);
  if (!found) {
    fail(
      `could not find ${EXE}.\n` +
        `  Install it with:  cargo install sakur4d\n` +
        `  Or download a release: https://github.com/sakur4/sakur4/releases\n` +
        `  Or point at it with:  --bin /path/to/${EXE}   (or SAKUR4_BIN)`,
    );
  }
  return found;
}

// ---------------------------------------------------------------------------
// Store and session defaults
// ---------------------------------------------------------------------------

function defaultStore() {
  const fromEnv = process.env.SAKUR4_DB;
  if (fromEnv) return fromEnv;
  // `~/.sakur4/` rather than the working directory: a skill is loaded in whatever
  // project the user happens to be in, and a store per working directory would
  // fragment memory in a way nobody asked for.
  return join(homedir(), ".sakur4", "sakur4.db");
}

function ensureStoreDir(store) {
  if (store === ":memory:" || store.startsWith("file:")) return;
  const dir = dirname(store);
  if (!existsSync(dir)) {
    mkdirSync(dir, { recursive: true });
  }
}

/**
 * Default session id: the working directory's basename.
 *
 * A skill cannot invent a session id that matches whatever the harness uses, so it
 * picks something stable and obvious. `SAKUR4_SESSION` overrides it, and so does
 * `--session`, which is what a harness adapter should pass.
 */
function defaultSession() {
  if (process.env.SAKUR4_SESSION) return process.env.SAKUR4_SESSION;
  const base = process.cwd().split(/[\\/]/).filter(Boolean).pop();
  return base ? `skill-${base}` : "skill-default";
}

// ---------------------------------------------------------------------------
// Running the daemon
// ---------------------------------------------------------------------------

function run(deps, args, { capture = true } = {}) {
  const result = spawnSync(deps.daemon.command, args, {
    encoding: "utf8",
    // Arguments are passed as an array, never through a shell, so a content
    // argument may contain anything at all.
    shell: false,
    maxBuffer: 64 * 1024 * 1024,
    stdio: capture ? ["ignore", "pipe", "pipe"] : "inherit",
  });

  if (result.error) {
    fail(`could not run ${deps.daemon.command}: ${result.error.message}`);
  }
  const stdout = result.stdout ?? "";
  const stderr = result.stderr ?? "";
  if (result.status !== 0) {
    fail(
      `${deps.daemon.command} ${args.slice(0, 2).join(" ")} exited ${result.status}` +
        (stderr.trim() ? `\n${stderr.trim()}` : "") +
        (stdout.trim() ? `\n${stdout.trim()}` : ""),
    );
  }
  return { stdout, stderr };
}

/** Global flags every subcommand accepts, placed before the subcommand. */
function globalArgs(deps) {
  const args = ["--db", deps.store];
  if (process.env.SAKUR4_BACKEND) {
    args.push("--backend", process.env.SAKUR4_BACKEND);
  }
  if (deps.projectRoot) {
    args.push("--project-root", deps.projectRoot);
  }
  return args;
}

// ---------------------------------------------------------------------------
// Output helpers
// ---------------------------------------------------------------------------

function emitJson(value) {
  process.stdout.write(`${JSON.stringify(value, null, 2)}\n`);
}

function readContentArg(params, usage) {
  const sources = [params.content, params.file].filter((v) => v !== undefined);
  if (sources.length === 0) {
    fail("either --content or --file is required", usage);
  }
  if (sources.length > 1) {
    fail("--content and --file are mutually exclusive", usage);
  }
  if (params.file !== undefined) {
    const path = resolve(params.file);
    if (!existsSync(path)) fail(`--file points at ${path}, which does not exist`);
    // Read as UTF-8 rather than streaming: a tool result is text by definition here,
    // and the daemon takes a string. A binary file is a mistake worth naming.
    const stats = statSync(path);
    if (!stats.isFile()) fail(`--file points at ${path}, which is not a file`);
    return readFileSync(path, "utf8");
  }
  return params.content;
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

const USAGE = `sakur4 — memory and context for a long agent session

  sakur4 doctor                          what the daemon detected
  sakur4 commit --role <r> ...           record a turn or tool result
  sakur4 pin --kind <k> --content <c>    pin a constraint so it survives compaction
  sakur4 recall --query <q>              find earlier work by meaning
  sakur4 receipt                         where the context budget went
  sakur4 usage --prompt-tokens <n>       report provider usage and get a cache verdict
  sakur4 fold --description <d> --goal <g>
  sakur4 unfold --fold-id <id> --summary <s>
  sakur4 recall-fold --fold-id <id>      the full trace of a folded subtask
  sakur4 index --root <path>             build the code graph
  sakur4 map [--budget <n>]              structural outline of the repository
  sakur4 symbol --name <n>               a symbol's current signature
  sakur4 impact --name <n>               every call site that depends on a symbol
  sakur4 staleness                       summaries that no longer match their source
  sakur4 dream                           run one memory-maintenance pass

Global: --session <id>  --bin <path>  --db <path>  --root <path>  --json
Run 'sakur4 <command> --help' for a command's own options.`;

const COMMANDS = {
  help: {
    usage: USAGE,
    run() {
      process.stdout.write(`${USAGE}\n`);
    },
  },

  doctor: {
    usage: "sakur4 doctor [--json]",
    flags: { json: "bool", refresh: "bool" },
    run(deps, params) {
      const args = [...globalArgs(deps), "doctor"];
      if (params.refresh) args.push("--refresh");
      const { stdout } = run(deps, args);
      if (params.json) {
        emitJson({
          daemon: deps.daemon.command,
          resolvedVia: deps.daemon.how,
          store: deps.store,
          report: stdout.trim(),
        });
        return;
      }
      process.stdout.write(
        `sakur4 daemon: ${deps.daemon.command}  (found via ${deps.daemon.how})\n` +
          `sakur4 store:  ${deps.store}\n\n${stdout}`,
      );
    },
  },

  commit: {
    usage:
      "sakur4 commit --role <system|user|assistant|tool> (--content <text> | --file <path>) [--tool <name>] [--session <id>] [--json]",
    flags: {
      role: "value",
      content: "value",
      file: "value",
      tool: "value",
      session: "value",
      json: "bool",
    },
    run(deps, params) {
      if (!params.role) fail("--role is required (system, user, assistant, tool)", this.usage);
      const content = readContentArg(params, this.usage);
      const session = params.session ?? deps.session;

      const args = [
        ...globalArgs(deps),
        "commit",
        session,
        content,
        "--role",
        params.role,
      ];
      if (params.tool) args.push("--tool", params.tool);

      const { stdout } = run(deps, args);
      if (params.json) {
        emitJson({ session, role: params.role, output: stdout.trim() });
        return;
      }
      process.stdout.write(stdout);
    },
  },

  pin: {
    usage:
      "sakur4 pin --kind <safety_constraint|user_correction|task_contract> --content <text> [--session <id>] [--json]",
    flags: { kind: "value", content: "value", file: "value", session: "value", json: "bool" },
    run(deps, params) {
      const kind = params.kind ?? "task_contract";
      const content = readContentArg(params, this.usage);
      const session = params.session ?? deps.session;

      const args = [...globalArgs(deps), "pin", content, "--kind", kind, "--session", session];
      const { stdout } = run(deps, args);
      if (params.json) {
        emitJson({ session, kind, pinned: true, output: stdout.trim() });
        return;
      }
      process.stdout.write(stdout);
    },
  },

  recall: {
    usage: "sakur4 recall --query <text> [--k <n>] [--session <id>] [--folded] [--json]",
    flags: {
      query: "value",
      k: "value",
      session: "value",
      folded: "bool",
      json: "bool",
    },
    run(deps, params) {
      if (!params.query) fail("--query is required", this.usage);
      const session = params.session ?? deps.session;
      const args = [...globalArgs(deps), "recall", params.query, "--k", params.k ?? "8", "--session", session];
      if (params.folded) args.push("--include-folded");

      const { stdout, stderr } = run(deps, args);
      if (params.json) {
        emitJson({ query: params.query, session, output: stdout.trim(), notes: stderr.trim() });
        return;
      }
      process.stdout.write(stdout);
      if (stderr.trim()) process.stderr.write(stderr);
    },
  },

  receipt: {
    usage: "sakur4 receipt [--session <id>] [--history] [--limit <n>] [--json]",
    flags: { session: "value", history: "bool", limit: "value", json: "bool" },
    run(deps, params) {
      const session = params.session ?? deps.session;
      const args = [...globalArgs(deps), "receipt", session, "--limit", params.limit ?? "20"];
      if (params.history) args.push("--history");
      const { stdout } = run(deps, args);
      if (params.json) {
        emitJson({ session, output: stdout.trim() });
        return;
      }
      process.stdout.write(stdout);
    },
  },

  usage: {
    usage:
      "sakur4 usage --prompt-tokens <n> [--completion-tokens <n>] [--cache-read-tokens <n>] [--cache-write-tokens <n>] [--reasoning-tokens <n>] [--provider <p>] [--model <m>] [--session <id>] [--json]",
    flags: {
      "prompt-tokens": "value",
      "completion-tokens": "value",
      "cache-read-tokens": "value",
      "cache-write-tokens": "value",
      "reasoning-tokens": "value",
      provider: "value",
      model: "value",
      session: "value",
      json: "bool",
    },
    run(deps, params) {
      const prompt = params["prompt-tokens"];
      if (prompt === undefined) fail("--prompt-tokens is required", this.usage);
      // Validated here rather than inside the daemon so a typo like `--prompt-tokens
      // 6,200` is caught before it becomes a request the daemon rejects obscurely.
      for (const key of [
        "prompt-tokens",
        "completion-tokens",
        "cache-read-tokens",
        "cache-write-tokens",
        "reasoning-tokens",
      ]) {
        const value = params[key];
        if (value !== undefined && !/^\d+$/.test(value)) {
          fail(`--${key} must be a non-negative integer, got ${JSON.stringify(value)}`);
        }
      }
      const session = params.session ?? deps.session;

      // The daemon exposes this over MCP rather than the CLI, so drive it the same
      // way an MCP client would: a single JSON-RPC call over stdio. That keeps the
      // skill and the MCP surface on one code path instead of two that can drift.
      const toolArgs = {
        prompt_tokens: Number(prompt),
        completion_tokens: Number(params["completion-tokens"] ?? 0),
        session_id: session,
      };
      // Absent means absent: only forward a cache figure the caller actually got.
      for (const [flag, field] of [
        ["cache-read-tokens", "cache_read_tokens"],
        ["cache-write-tokens", "cache_write_tokens"],
        ["reasoning-tokens", "reasoning_tokens"],
      ]) {
        if (params[flag] !== undefined) toolArgs[field] = Number(params[flag]);
      }
      if (params.provider) toolArgs.provider = params.provider;
      if (params.model) toolArgs.model = params.model;

      const result = callMcpTool(deps, "context.record_usage", toolArgs);
      if (params.json) {
        emitJson(result);
        return;
      }
      process.stdout.write(`verdict: ${result.verdict}\n  ${result.headline}\n`);
      if (result.detail) process.stdout.write(`  ${result.detail}\n`);
      if (result.session_stats) process.stdout.write(`  ${result.session_stats}\n`);
      if (result.regression) {
        process.stderr.write(
          "\nwarning: this turn was billed for history that had already been paid for.\n",
        );
      }
    },
  },

  fold: {
    usage: "sakur4 fold --description <text> --goal <text> [--session <id>] [--slot <id>] [--json]",
    flags: {
      description: "value",
      goal: "value",
      session: "value",
      slot: "value",
      json: "bool",
    },
    run(deps, params) {
      if (!params.description) fail("--description is required", this.usage);
      if (!params.goal) fail("--goal is required", this.usage);
      const result = callMcpTool(deps, "memory.fold", {
        description: params.description,
        goal: params.goal,
        session_id: params.session ?? deps.session,
        slot_id: params.slot ?? "0",
      });
      if (params.json) {
        emitJson(result);
        return;
      }
      process.stdout.write(`fold opened: ${result.fold_id}\n  ${result.cache_note ?? ""}\n`);
      if (result.next_steps) process.stdout.write(`  ${result.next_steps}\n`);
    },
  },

  unfold: {
    usage: "sakur4 unfold --fold-id <id> --summary <text> [--session <id>] [--slot <id>] [--json]",
    flags: {
      "fold-id": "value",
      summary: "value",
      session: "value",
      slot: "value",
      json: "bool",
    },
    run(deps, params) {
      if (!params["fold-id"]) fail("--fold-id is required", this.usage);
      if (!params.summary) {
        // Required on purpose: the whole point of a fold is that the trace collapses
        // to a result, and an empty result loses all of it.
        fail("--summary is required — the fold collapses to this text", this.usage);
      }
      const result = callMcpTool(deps, "memory.unfold", {
        fold_id: params["fold-id"],
        result_summary: params.summary,
        session_id: params.session ?? deps.session,
        slot_id: params.slot ?? "0",
      });
      if (params.json) {
        emitJson(result);
        return;
      }
      process.stdout.write(
        `fold closed: ${result.fold_id}\n` +
          `  ${result.episodes_folded} episode(s) collapsed, ${result.tokens_reclaimed} tokens reclaimed\n` +
          `  rollback: ${result.rollback_detail ?? "not attempted"}\n` +
          `  full trace still retrievable: ${result.trace_retrievable}\n`,
      );
    },
  },

  "recall-fold": {
    usage: "sakur4 recall-fold --fold-id <id> [--json]",
    flags: { "fold-id": "value", json: "bool" },
    run(deps, params) {
      if (!params["fold-id"]) fail("--fold-id is required", this.usage);
      const result = callMcpTool(deps, "memory.recall_fold", { fold_id: params["fold-id"] });
      if (params.json) {
        emitJson(result);
        return;
      }
      process.stdout.write(
        `fold ${result.fold_id} (${result.status}) — ${result.description}\n` +
          `  goal: ${result.goal}\n` +
          `  result: ${result.result_summary ?? "(none)"}\n\n`,
      );
      for (const ep of result.episodes ?? []) {
        process.stdout.write(`[${ep.seq}] ${ep.role}${ep.tool_name ? ` (${ep.tool_name})` : ""}\n${ep.content}\n\n`);
      }
    },
  },

  index: {
    usage: "sakur4 index [--root <path>] [--full] [--json]",
    flags: { root: "value", full: "bool", json: "bool" },
    run(deps, params) {
      const root = params.root ?? deps.projectRoot ?? process.cwd();
      const args = [...globalArgs(deps), "index", root];
      if (params.full) args.push("--full");
      const { stdout } = run(deps, args);
      if (params.json) {
        emitJson({ root, output: stdout.trim() });
        return;
      }
      process.stdout.write(stdout);
    },
  },

  map: {
    usage: "sakur4 map [--budget <n>] [--focus <path>]... [--json]",
    flags: { budget: "value", focus: "list", json: "bool" },
    run(deps, params) {
      const args = [...globalArgs(deps), "repo-map", "--budget", params.budget ?? "2000"];
      for (const focus of params.focus ?? []) args.push("--focus", focus);
      const { stdout } = run(deps, args);
      if (params.json) {
        emitJson({ budget: Number(params.budget ?? 2000), map: stdout.trim() });
        return;
      }
      process.stdout.write(stdout);
    },
  },

  symbol: {
    usage: "sakur4 symbol --name <qualified-name> [--json]",
    flags: { name: "value", json: "bool" },
    run(deps, params) {
      if (!params.name) fail("--name is required (e.g. src::auth::validate)", this.usage);
      const result = callMcpTool(deps, "code.query_symbol", { qualified_name: params.name });
      if (params.json) {
        emitJson(result);
        return;
      }
      if (!result.found) {
        process.stdout.write(`not found: ${params.name}\n  ${result.note ?? ""}\n`);
        return;
      }
      process.stdout.write(
        `${result.qualified_name}  ${result.signature ?? ""}\n` +
          `  kind: ${result.kind}  file: ${result.file ?? "?"}:${result.line ?? "?"}\n` +
          `  ast_hash: ${result.ast_hash}\n`,
      );
    },
  },

  impact: {
    usage: "sakur4 impact --name <qualified-name> [--depth <n>] [--json]",
    flags: { name: "value", depth: "value", json: "bool" },
    run(deps, params) {
      if (!params.name) fail("--name is required", this.usage);
      const result = callMcpTool(deps, "code.impact_of_change", {
        qualified_name: params.name,
        depth: params.depth ? Number(params.depth) : 4,
      });
      if (params.json) {
        emitJson(result);
        return;
      }
      process.stdout.write(result.rendered ?? JSON.stringify(result, null, 2));
    },
  },

  staleness: {
    usage: "sakur4 staleness [--json]",
    flags: { json: "bool" },
    run(deps, params) {
      const result = callMcpTool(deps, "memory.staleness", {});
      if (params.json) {
        emitJson(result);
        return;
      }
      process.stdout.write(
        `${result.stale} of ${result.total} stored summaries no longer match their source` +
          ` (${(result.stale_rate * 100).toFixed(0)}%)\n`,
      );
      for (const entry of result.entries ?? []) {
        process.stdout.write(`  ${entry.anchor_type}(${entry.anchor_id}): ${entry.reason}\n`);
      }
    },
  },

  dream: {
    usage: "sakur4 dream [--force] [--json]",
    flags: { force: "bool", json: "bool" },
    run(deps, params) {
      const result = callMcpTool(deps, "sakur4.dream", { force: params.force === true });
      if (params.json) {
        emitJson(result);
        return;
      }
      process.stdout.write(`${result.summary}\n`);
      for (const note of result.notes ?? []) process.stdout.write(`  ${note}\n`);
    },
  },
};

// ---------------------------------------------------------------------------
// Minimal MCP stdio client
// ---------------------------------------------------------------------------

/**
 * Call one MCP tool over stdio and return its structured result.
 *
 * The daemon's CLI covers most of what this skill needs, but a few operations are
 * only exposed as tools — `context.record_usage` among them, because it exists for
 * harnesses rather than for humans. Rather than add a parallel CLI path in the
 * daemon and keep two implementations in step, this speaks the protocol directly.
 *
 * The framing is newline-delimited JSON-RPC, which is what the stdio transport
 * uses. The handshake is the legacy `initialize` form: it is what a plain stdio
 * client sends, and the daemon answers it.
 */
function callMcpTool(deps, toolName, toolArgs) {
  const frames = [
    {
      jsonrpc: "2.0",
      id: 1,
      method: "initialize",
      params: {
        protocolVersion: "2025-11-25",
        capabilities: {},
        clientInfo: { name: "sakur4-skill", version: "0.1.0" },
      },
    },
    { jsonrpc: "2.0", method: "notifications/initialized", params: {} },
    {
      jsonrpc: "2.0",
      id: 2,
      method: "tools/call",
      params: { name: toolName, arguments: toolArgs },
    },
  ];

  const result = spawnSync(
    deps.daemon.command,
    [...globalArgs(deps), "serve", "--transport", "stdio", "--no-dream"],
    {
      encoding: "utf8",
      shell: false,
      input: `${frames.map((f) => JSON.stringify(f)).join("\n")}\n`,
      maxBuffer: 64 * 1024 * 1024,
      // stdin ends when `input` is written, which is what makes the daemon exit.
      timeout: 60_000,
    },
  );
  void spawnSync;

  if (result.error) {
    fail(`could not talk to ${deps.daemon.command}: ${result.error.message}`);
  }
  const stdout = result.stdout ?? "";
  let toolError;
  for (const line of stdout.split("\n")) {
    const trimmed = line.trim();
    // A non-JSON line here is a bug worth reporting rather than skipping: over
    // stdio, stdout carries protocol frames and nothing else.
    if (!trimmed.startsWith("{")) {
      if (trimmed.length > 0) {
        process.stderr.write(`sakur4: ignoring non-JSON stdout: ${trimmed}\n`);
      }
      continue;
    }
    let parsed;
    try {
      parsed = JSON.parse(trimmed);
    } catch {
      process.stderr.write(`sakur4: unparsable stdout frame: ${trimmed}\n`);
      continue;
    }
    if (parsed.id !== 2) continue;
    if (parsed.error) {
      fail(`${toolName} failed: ${parsed.error.message ?? JSON.stringify(parsed.error)}`);
    }
    if (parsed.result?.structuredContent) {
      return parsed.result.structuredContent;
    }
    // Fall back to the text content, which is the same JSON for these tools.
    const text = (parsed.result?.content ?? [])
      .filter((c) => c.type === "text")
      .map((c) => c.text)
      .join("");
    try {
      return JSON.parse(text);
    } catch {
      fail(`${toolName} returned content that was not JSON: ${text.slice(0, 200)}`);
    }
  }
  void toolError;
  fail(
    `${toolName} produced no reply.\n` +
      `stderr: ${(result.stderr ?? "").trim().slice(0, 500)}`,
  );
}

function mcpRequire() {
  return { spawnSync };
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

function main() {
  const argv = process.argv.slice(2);
  const name = argv[0];
  if (!name || name === "--help" || name === "-h") {
    process.stdout.write(`${USAGE}\n`);
    return;
  }

  const command = COMMANDS[name];
  if (!command) {
    fail(`unknown command '${name}'`, USAGE);
  }
  if (argv.includes("--help") || argv.includes("-h")) {
    process.stdout.write(`${command.usage}\n`);
    return;
  }

  const params = parseArgs(argv.slice(1), {
    flags: command.flags ?? {},
    usage: command.usage,
  });

  const store = params.db ?? defaultStore();
  ensureStoreDir(store);

  const deps = {
    daemon: requireDaemon(params),
    store,
    session: params.session ?? defaultSession(),
    projectRoot: params.root ? (isAbsolute(params.root) ? params.root : resolve(params.root)) : undefined,
  };

  command.run(deps, params);
}

main();
