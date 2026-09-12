/**
 * Sakur4 for Oh My Pi — native memory and context management.
 *
 * # What this extension is for
 *
 * Sakur4 is a memory and context layer. Over MCP it is reachable from any harness,
 * but OMP has no MCP client, and a harness that *can* see inside the agent loop can
 * do more than call tools. That extra reach is what this file uses:
 *
 * | Hook | What Sakur4 does with it |
 * |---|---|
 * | `before_agent_start` | injects the working preamble, so the model knows when to fold and when to pin |
 * | `context` | retrieves relevant memory for the prompt and prepends it, then records its own footprint so the budget stays honest |
 * | `message_end` | reports the provider's token usage, so prompt-cache behaviour is measured every turn rather than only when someone asks |
 * | `session_before_compact` | replaces blind summarisation with Sakur4's planned eviction, and reports what it cost the cache |
 * | `session_shutdown` | flushes and says what the session cost |
 *
 * # The rule this file follows
 *
 * Sakur4 is a subsystem, not a second agent (the PRD's own non-goal). So every hook
 * here either observes, retrieves, or defers to the daemon's decision — and every
 * hook has a path that does nothing at all when the daemon is not reachable. An
 * extension that breaks a session because a sidecar is down is worse than no
 * extension; `sakur4d` is discovered lazily and its absence is reported once, not
 * per turn.
 *
 * # No runtime dependencies
 *
 * Only `node:` builtins and the peer type-imports. A plugin installed with
 * `npm install --omit=dev` cannot rely on devDependencies, and `typebox` lives
 * nested inside OMP's own tree rather than at a path this package can resolve. Tool
 * schemas are therefore written as plain JSON Schema objects, which is the shape the
 * host serialises anyway.
 */

import { spawnSync } from "node:child_process";
import { appendFileSync, existsSync, mkdirSync } from "node:fs";
import { homedir, platform } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import type {
  ExtensionAPI,
  ExtensionCommandContext,
  ExtensionContext,
} from "@earendil-works/pi-coding-agent";

const EXE = platform() === "win32" ? "sakur4d.exe" : "sakur4d";

// ===========================================================================
// Configuration
// ===========================================================================

interface Config {
  /** Where the daemon lives, or undefined to search. */
  bin?: string;
  /** Memory store path. */
  db: string;
  /** Session id reported to the daemon. */
  session: string;
  /** Project root for Repo Cortex. */
  projectRoot: string;
  /** Inject retrieved memory before each turn. */
  retrieve: boolean;
  /** Report provider usage automatically each turn. */
  reportUsage: boolean;
  /** Take over compaction. */
  ownCompaction: boolean;
  /** Tokens of retrieved memory to inject at most. */
  recallBudget: number;
}

function readConfig(cwd: string): Config {
  const bool = (name: string, fallback: boolean) => {
    const raw = process.env[name];
    if (raw === undefined || raw === "") return fallback;
    return !/^(0|false|no|off)$/i.test(raw);
  };
  const int = (name: string, fallback: number) => {
    const parsed = Number.parseInt(process.env[name] ?? "", 10);
    return Number.isFinite(parsed) && parsed > 0 ? parsed : fallback;
  };
  return {
    bin: process.env.SAKUR4_BIN,
    db: process.env.SAKUR4_DB ?? join(homedir(), ".sakur4", "sakur4.db"),
    // A stable id per project, so memory does not bleed between repositories and
    // does not require the operator to configure anything.
    session: process.env.SAKUR4_SESSION ?? `omp-${basename(cwd)}`,
    projectRoot: process.env.SAKUR4_PROJECT_ROOT ?? cwd,
    retrieve: bool("SAKUR4_RETRIEVE", true),
    reportUsage: bool("SAKUR4_REPORT_USAGE", true),
    ownCompaction: bool("SAKUR4_OWN_COMPACTION", true),
    recallBudget: int("SAKUR4_RECALL_BUDGET", 1200),
  };
}

function basename(path: string): string {
  const parts = path.split(/[\\/]/).filter(Boolean);
  return parts.pop() ?? "default";
}

/**
 * Append a line to the plugin's diagnostic log, when enabled.
 *
 * A plugin that silently does nothing is indistinguishable from a plugin that
 * failed to load, and OMP surfaces extension-load errors only to a TTY. This log is
 * how the extension is verified from a scripted run: set `SAKUR4_PLUGIN_LOG` to a
 * path and each lifecycle step appends a line. Off by default — writing to disk on
 * every session would be rude.
 */
function diagnose(step: string, detail?: Record<string, unknown>): void {
  const path = process.env.SAKUR4_PLUGIN_LOG;
  if (!path) return;
  try {
    appendFileSync(
      path,
      `${new Date().toISOString()} ${step}${detail ? ` ${JSON.stringify(detail)}` : ""}\n`,
    );
  } catch {
    // Diagnostics must never be the reason a session fails.
  }
}

// ===========================================================================
// Talking to the daemon
// ===========================================================================

interface Daemon {
  command: string;
  how: string;
}

/**
 * Everywhere `sakur4d` plausibly lives, in the order a user would expect it to be
 * found, each with a note on *why* it is on the list.
 *
 * The notes exist for the failure path. "No sakur4d binary found" is useless on its
 * own — it does not say where was searched, so the user cannot tell whether their
 * install landed somewhere unusual or did not happen at all. The warning below
 * prints this list.
 */
function searchPaths(configured?: string): Array<{ path: string; how: string }> {
  const home = homedir();
  const found: Array<{ path: string; how: string }> = [];

  if (configured) found.push({ path: configured, how: "SAKUR4_BIN" });

  // `~/.cargo/bin` is where `cargo install` puts a binary on every platform, and
  // where the Rust installer adds to PATH. It is the single most likely place a
  // correct install lands, which makes it the single most confusing place for the
  // search to have missed.
  found.push(
    { path: join(home, ".cargo", "bin", EXE), how: "cargo install" },
    { path: join(home, ".local", "bin", EXE), how: "~/.local/bin" },
    { path: join(home, ".sakur4", "bin", EXE), how: "~/.sakur4/bin" },
  );

  if (platform() === "win32") {
    found.push(
      { path: join(process.env.LOCALAPPDATA ?? "", "Sakur4", EXE), how: "%LOCALAPPDATA%\\Sakur4" },
      { path: join(home, "scoop", "shims", EXE), how: "scoop" },
      { path: join(process.env.ProgramData ?? "", "chocolatey", "bin", EXE), how: "chocolatey" },
    );
  } else {
    found.push(
      { path: `/usr/local/bin/${EXE}`, how: "/usr/local/bin" },
      { path: `/opt/homebrew/bin/${EXE}`, how: "homebrew (arm64)" },
      { path: `/usr/local/opt/sakur4/bin/${EXE}`, how: "homebrew (intel)" },
    );
  }

  // A source checkout, which is how this is run during development. Checked from
  // the working directory upward, because OMP is usually started at the repo root.
  found.push(
    { path: join(dirname(process.execPath), EXE), how: "beside the running node" },
    { path: join(process.cwd(), EXE), how: "the working directory" },
    { path: join(process.cwd(), "target", "release", EXE), how: "a checkout (release build)" },
    { path: join(process.cwd(), "target", "debug", EXE), how: "a checkout (debug build)" },
  );

  return found;
}

/**
 * Find `sakur4d` without assuming where it was installed.
 *
 * Returns null rather than throwing: a missing daemon disables the extension and
 * says so once, with the search list. That is the difference between "this session
 * has no memory" and "this session is broken".
 */
function findDaemon(configured?: string): Daemon | null {
  for (const candidate of searchPaths(configured)) {
    if (candidate.path && existsSync(candidate.path)) {
      return { command: candidate.path, how: candidate.how };
    }
  }

  // Finally, a bare invocation so the OS resolves PATH itself. Last because it
  // costs a process spawn and every check above is free. `shell: false` keeps a
  // stray `sakur4d.bat` from being interpreted by a shell.
  const probe = spawnSync(EXE, ["--version"], { encoding: "utf8", timeout: 10_000, shell: false });
  if (!probe.error && probe.status === 0) {
    return { command: EXE, how: "PATH" };
  }

  return null;
}

class Client {
  readonly config: Config;
  private daemon: Daemon | null;
  private probed = false;
  private warned = false;

  constructor(config: Config) {
    this.config = config;
    this.daemon = findDaemon(config.bin);
    if (this.daemon) this.probed = true;
  }

  /** Whether a daemon was found. Cheap once probed; re-probes once on miss so
   *  installing `sakur4d` mid-session does not require a restart. */
  available(): Daemon | null {
    if (!this.daemon && this.probed === false) {
      this.daemon = findDaemon(this.config.bin);
      this.probed = true;
    }
    return this.daemon;
  }

  /** Reset the probe so a later call looks again. */
  recheck(): Daemon | null {
    this.probed = false;
    return this.available();
  }

  private globalArgs(): string[] {
    const args = ["--db", this.config.db];
    if (process.env.SAKUR4_BACKEND) args.push("--backend", process.env.SAKUR4_BACKEND);
    if (this.config.projectRoot) args.push("--project-root", this.config.projectRoot);
    return args;
  }

  /**
   * Run a `sakur4d` subcommand and return its stdout, or null on any failure.
   *
   * Arguments go through as an array with `shell: false`, so content containing
   * quotes, newlines or backticks is passed intact. That matters because the main
   * use is recording the user's own words verbatim.
   */
  run(args: string[], stdin?: string): { stdout: string; stderr: string } | null {
    const daemon = this.available();
    if (!daemon) return null;
    const result = spawnSync(daemon.command, [...this.globalArgs(), ...args], {
      encoding: "utf8",
      shell: false,
      input: stdin,
      timeout: 120_000,
      maxBuffer: 64 * 1024 * 1024,
    });
    if (result.error || result.status !== 0) return null;
    return { stdout: result.stdout ?? "", stderr: result.stderr ?? "" };
  }

  /**
   * Call one MCP tool over stdio and return its structured result.
   *
   * A few operations exist only as MCP tools, because they were designed for
   * harnesses rather than for humans — `context.record_usage` and the fold family
   * among them. Speaking the protocol directly keeps one implementation in the
   * daemon rather than two that can drift.
   */
  tool(name: string, toolArgs: Record<string, unknown>): Record<string, unknown> | null {
    const daemon = this.available();
    if (!daemon) return null;
    const frames = [
      {
        jsonrpc: "2.0",
        id: 1,
        method: "initialize",
        params: {
          protocolVersion: "2025-11-25",
          capabilities: {},
          clientInfo: { name: "omp-sakur4", version: "0.1.0" },
        },
      },
      { jsonrpc: "2.0", method: "notifications/initialized", params: {} },
      {
        jsonrpc: "2.0",
        id: 2,
        method: "tools/call",
        params: { name, arguments: toolArgs },
      },
    ];
    const result = spawnSync(
      daemon.command,
      [...this.globalArgs(), "serve", "--transport", "stdio", "--no-dream"],
      {
        encoding: "utf8",
        shell: false,
        input: `${frames.map((f) => JSON.stringify(f)).join("\n")}\n`,
        timeout: 120_000,
        maxBuffer: 64 * 1024 * 1024,
      },
    );
    if (result.error || !result.stdout) return null;

    for (const line of result.stdout.split("\n")) {
      const trimmed = line.trim();
      // Over stdio, stdout carries protocol frames and nothing else, so a
      // non-JSON line is a bug worth surfacing rather than skipping silently.
      if (!trimmed.startsWith("{")) continue;
      let parsed: any;
      try {
        parsed = JSON.parse(trimmed);
      } catch {
        continue;
      }
      if (parsed.id !== 2) continue;
      if (parsed.error) return null;
      if (parsed.result?.structuredContent) return parsed.result.structuredContent;
      const text = (parsed.result?.content ?? [])
        .filter((c: any) => c.type === "text")
        .map((c: any) => c.text)
        .join("");
      try {
        return JSON.parse(text);
      } catch {
        return null;
      }
    }
    return null;
  }

  /**
   * Report the daemon's absence once per session, with enough detail to act on.
   *
   * # Why the message is long
   *
   * The first version said only "no sakur4d binary found, so memory is disabled".
   * That is true and nearly useless: it does not say where was searched, so the
   * user cannot tell whether their install landed somewhere unusual or never
   * happened. The most likely case by far is a correct `cargo install` into
   * `~/.cargo/bin` that this plugin failed to look in — and the fix for *that* is a
   * bug fix here, not a user action.
   *
   * So the message names the search list, the three ways to install, and the two
   * ways to point at an existing binary. A warning a user cannot act on is noise.
   *
   * # Why it is reported per session, not per turn
   *
   * It fires from `session_start`, so it appears once. A notification on every turn
   * would be worse than the missing feature; a notification never shown would leave
   * the user wondering why nothing is being remembered.
   */
  warnOnce(ctx: ExtensionContext | ExtensionCommandContext): void {
    if (this.warned || this.available()) return;
    this.warned = true;

    const searched = searchPaths(this.config.bin)
      .filter((candidate) => candidate.path)
      .map((candidate) => `    ${candidate.path}   (${candidate.how})`)
      .join("\n");

    ctx.ui.notify(
      [
        "Sakur4: no sakur4d binary found, so memory is disabled for this session.",
        "",
        "Install it:",
        "    cargo install sakur4d",
        "    or download a release binary and put it on PATH",
        "",
        "Or point at an existing build: set SAKUR4_BIN to the binary's path.",
        "",
        "Searched:",
        searched,
      ].join("\n"),
      "warning",
    );

    diagnose("daemon not found", { searched: searchPaths(this.config.bin).map((c) => c.path) });
  }

  ensureStoreDir(): void {
    const store = this.config.db;
    if (store === ":memory:" || store.startsWith("file:")) return;
    const dir = dirname(store);
    if (!existsSync(dir)) {
      try {
        mkdirSync(dir, { recursive: true });
      } catch {
        // A store directory that cannot be created surfaces when the daemon runs;
        // failing here would take down the extension for a cosmetic reason.
      }
    }
  }
}

// ===========================================================================
// The preamble
// ===========================================================================

/**
 * Injected once per session via `before_agent_start`.
 *
 * It names *when* to call each tool rather than describing what they do, because
 * the failure mode with smaller instruction-tuned models is under-triggering: they
 * have the tools and do not use them. Numbered triggers are what fixed that in
 * testing; a prose description did not.
 */
const PREAMBLE = `## Sakur4 memory (active)

You have a persistent memory and context layer for this session. Use it.

1. **After each turn**, call \`sakur4_commit\` with what happened. Pass the user's own
   words verbatim for their turns — do not paraphrase their intent. For tool results,
   pass \`toolName\` so structured output (JSON, CSV, diffs, exit codes) is parsed
   into facts rather than stored as prose.

2. **The moment the user states a rule, corrects you, or sets a hard requirement**,
   call \`sakur4_pin\` for it. Unpinned requirements are compacted away; pinned ones
   cannot be. This is the single highest-value habit here.

3. **Before a subtask you expect to take many steps** (reading many files, running
   many searches, trying an approach you may abandon), call \`sakur4_fold\` first and
   \`sakur4_unfold\` with a result summary when done. The intermediate steps then
   leave your context window entirely, and the work still happened.

4. **When you cannot remember something, look it up instead of reconstructing it**:
   \`sakur4_recall\` for anything, \`sakur4_symbol\` for a function's current signature.
   A recall result marked STALE is a summary whose source has changed — use the
   CURRENT VALUE attached to it, never the summary.

5. **Before changing a signature**, call \`sakur4_impact\` to see every call site that
   depends on it.

6. **If a turn felt slow**, call \`sakur4_receipt\` to see where the context budget
   went and whether the prompt had to be reprocessed from scratch.`;

// ===========================================================================
// Tool schemas
// ===========================================================================
//
// Plain JSON Schema, not a builder. `typebox` is a transitive dependency of OMP's
// own tree rather than something this package can resolve at runtime, and a plugin
// that fails to load because of a schema library is a plugin nobody can use. The
// host serialises these straight to the provider, so the literal form is the
// contract.

const str = (description: string) => ({ type: "string", description });
const int = (description: string) => ({ type: "integer", description });

const enumOf = (values: string[], description: string) => ({
  type: "string",
  enum: values,
  description,
});

// ===========================================================================
// Extension
// ===========================================================================

export default function sakur4(pi: ExtensionAPI): void {
  const config = readConfig(process.cwd());
  const client = new Client(config);
  client.ensureStoreDir();
  diagnose("factory invoked", {
    cwd: process.cwd(),
    session: config.session,
    daemon: client.available()?.command ?? null,
  });

  /** Turn counter, used to make the fold/unfold advice timely and to keep the
   *  receipt's turn numbering aligned with the daemon's. */
  let turns = 0;
  /** Whether the preamble has been injected for this session. */
  let preambleSent = false;
  /** The last retrieval we injected, so its footprint can be excluded from the
   *  usage we report on the next turn. Reporting usage that includes our own
   *  injection would make Sakur4's cost look like the model's. */
  let lastRetrievalTokens = 0;

  // -------------------------------------------------------------------------
  // Tools
  // -------------------------------------------------------------------------

  const text = (value: unknown) => ({
    content: [{ type: "text" as const, text: typeof value === "string" ? value : JSON.stringify(value, null, 2) }],
    details: {},
  });

  const unavailable = (what: string) =>
    text(
      `Sakur4 is unavailable (${what}): no sakur4d binary was found. ` +
        `Install it with \`cargo install sakur4d\` or set SAKUR4_BIN.`,
    );

  pi.registerTool({
    name: "sakur4_commit",
    label: "Sakur4: record",
    description:
      "Record a turn or tool result in Sakur4's append-only memory. Nothing is ever rewritten, so anything recorded can be recalled verbatim later — including after compaction has removed it from your context window. Structured tool output is additionally parsed into deterministic facts.",
    promptSnippet: "Record a turn or tool result in persistent memory",
    promptGuidelines: [
      "Use sakur4_commit after each turn to record what happened, passing the user's own words verbatim for their turns.",
      "Use sakur4_commit with toolName set when recording a tool result, so structured output becomes searchable facts rather than prose.",
    ],
    parameters: {
      type: "object",
      properties: {
        role: enumOf(["system", "user", "assistant", "tool"], "Who produced this content."),
        content: str("The content, verbatim."),
        toolName: str("Name of the tool that produced this content, when it is a tool result."),
      },
      required: ["role", "content"],
      additionalProperties: false,
    },
    async execute(_id, params: any) {
      if (!client.available()) return unavailable("commit");
      const args = ["commit", config.session, String(params.content), "--role", String(params.role)];
      if (params.toolName) args.push("--tool", String(params.toolName));
      const result = client.run(args);
      if (!result) return unavailable("commit");
      return text(result.stdout.trim() || "recorded");
    },
  });

  pi.registerTool({
    name: "sakur4_pin",
    label: "Sakur4: pin",
    description:
      "Pin a rule, correction or requirement into Sakur4's Anchor Set. Pinned content is rendered verbatim into every prompt and is exempt from every eviction tier, so it cannot be compacted away. Use this the moment the user states a standing rule or corrects you.",
    promptSnippet: "Pin a constraint so compaction cannot remove it",
    promptGuidelines: [
      "Use sakur4_pin immediately whenever the user states a rule, corrects you, or sets a hard requirement — unpinned requirements get compacted away.",
    ],
    parameters: {
      type: "object",
      properties: {
        content: str("The rule, correction or requirement, in the user's own terms."),
        kind: enumOf(
          ["safety_constraint", "user_correction", "task_contract"],
          "What kind of constraint this is.",
        ),
      },
      required: ["content", "kind"],
      additionalProperties: false,
    },
    async execute(_id, params: any) {
      if (!client.available()) return unavailable("pin");
      const result = client.run([
        "pin",
        String(params.content),
        "--kind",
        String(params.kind),
        "--session",
        config.session,
      ]);
      if (!result) return unavailable("pin");
      return text(result.stdout.trim() || "pinned");
    },
  });

  pi.registerTool({
    name: "sakur4_recall",
    label: "Sakur4: recall",
    description:
      "Search Sakur4's memory for earlier work, decisions or file contents. Results carry a STALE marker when the summary's source has since changed; a stale result includes the source's CURRENT VALUE, and you must use that instead of the summary. Prefer this over reconstructing an answer from memory.",
    promptSnippet: "Search persistent memory for earlier work and decisions",
    promptGuidelines: [
      "Use sakur4_recall when you cannot remember something rather than guessing; a result marked STALE carries the current value of its source, which supersedes the summary.",
    ],
    parameters: {
      type: "object",
      properties: {
        query: str("What to search for, in natural language."),
        limit: int("Maximum results. Default 8."),
      },
      required: ["query"],
      additionalProperties: false,
    },
    async execute(_id, params: any) {
      if (!client.available()) return unavailable("recall");
      const result = client.run([
        "recall",
        String(params.query),
        "--k",
        String(params.limit ?? 8),
        "--session",
        config.session,
      ]);
      if (!result) return unavailable("recall");
      return text(result.stdout.trim() || "no results");
    },
  });

  pi.registerTool({
    name: "sakur4_symbol",
    label: "Sakur4: symbol",
    description:
      "Look up a symbol's CURRENT signature, location and hash from Sakur4's parser-derived index. This is not memory and cannot be stale — it is the right way to check something you only remember from a summary or an earlier turn.",
    parameters: {
      type: "object",
      properties: {
        qualifiedName: str('The symbol, e.g. "src::auth::validate".'),
      },
      required: ["qualifiedName"],
      additionalProperties: false,
    },
    async execute(_id, params: any) {
      if (!client.available()) return unavailable("symbol");
      const result = client.tool("code.query_symbol", {
        qualified_name: String(params.qualifiedName),
      });
      if (!result) return unavailable("symbol");
      if (!result.found) {
        return text(`Not found: ${params.qualifiedName}. Run sakur4_index first, or check the spelling.`);
      }
      return text(
        `${result.qualified_name}  ${result.signature ?? ""}\n` +
          `  kind: ${result.kind}   file: ${result.file ?? "?"}:${result.line ?? "?"}\n` +
          `  ast_hash: ${result.ast_hash}`,
      );
    },
  });

  pi.registerTool({
    name: "sakur4_impact",
    label: "Sakur4: impact",
    description:
      "List every call site that depends on a symbol, transitively, from the pre-computed call and import graph. Call this before changing a signature, so the edit does not break callers you have not read.",
    promptSnippet: "Find every call site that depends on a symbol",
    promptGuidelines: [
      "Use sakur4_impact before changing a function's signature, so you know every caller that will break.",
    ],
    parameters: {
      type: "object",
      properties: {
        qualifiedName: str("The symbol to analyse."),
        depth: int("How many hops of callers to follow. Default 4."),
      },
      required: ["qualifiedName"],
      additionalProperties: false,
    },
    async execute(_id, params: any) {
      if (!client.available()) return unavailable("impact");
      const result = client.tool("code.impact_of_change", {
        qualified_name: String(params.qualifiedName),
        depth: Number(params.depth ?? 4),
      });
      if (!result) return unavailable("impact");
      return text(result.rendered ?? JSON.stringify(result, null, 2));
    },
  });

  pi.registerTool({
    name: "sakur4_fold",
    label: "Sakur4: fold",
    description:
      "Open an isolated sub-context for a subtask you expect to take many steps. Work done inside the fold leaves your context window when you close it, and the subtask collapses to a one-line result — while the full trace stays retrievable. Use it for exploration you may abandon.",
    promptSnippet: "Open an isolated sub-context for a multi-step subtask",
    promptGuidelines: [
      "Use sakur4_fold before a subtask that will take many steps — reading many files or running many searches — then sakur4_unfold with a result summary when it is done.",
    ],
    parameters: {
      type: "object",
      properties: {
        description: str("Short description of the subtask."),
        goal: str("What the subtask is trying to establish."),
      },
      required: ["description", "goal"],
      additionalProperties: false,
    },
    async execute(_id, params: any) {
      if (!client.available()) return unavailable("fold");
      const result = client.tool("memory.fold", {
        description: String(params.description),
        goal: String(params.goal),
        session_id: config.session,
        slot_id: "0",
      });
      if (!result) return unavailable("fold");
      return text(
        `Fold opened: ${result.fold_id}\n` +
          `  ${result.cache_note ?? ""}\n` +
          `  When finished, call sakur4_unfold with this id and a result summary.`,
      );
    },
  });

  pi.registerTool({
    name: "sakur4_unfold",
    label: "Sakur4: unfold",
    description:
      "Close a fold opened with sakur4_fold. Its intermediate steps leave the live window and only the result summary remains; the full trace stays retrievable. Requires a summary — that summary is the entire point of the fold.",
    parameters: {
      type: "object",
      properties: {
        foldId: str("The fold id returned by sakur4_fold."),
        summary: str("The subtask's result, in one or two sentences."),
      },
      required: ["foldId", "summary"],
      additionalProperties: false,
    },
    async execute(_id, params: any) {
      if (!client.available()) return unavailable("unfold");
      const result = client.tool("memory.unfold", {
        fold_id: String(params.foldId),
        result_summary: String(params.summary),
        session_id: config.session,
        slot_id: "0",
      });
      if (!result) return unavailable("unfold");
      return text(
        `Fold closed: ${result.fold_id}\n` +
          `  ${result.episodes_folded} episode(s) collapsed, ${result.tokens_reclaimed} tokens reclaimed\n` +
          `  rollback: ${result.rollback_detail ?? "not attempted"}`,
      );
    },
  });

  pi.registerTool({
    name: "sakur4_receipt",
    label: "Sakur4: receipt",
    description:
      "Show where this turn's context budget went, category by category, and the cache verdict: whether the prompt reused the provider's cached prefix or was reprocessed from scratch. Use it when a turn felt slow or expensive.",
    parameters: {
      type: "object",
      properties: {},
      additionalProperties: false,
    },
    async execute() {
      if (!client.available()) return unavailable("receipt");
      const result = client.run(["receipt", config.session]);
      if (!result) return unavailable("receipt");
      return text(result.stdout.trim() || "no receipt recorded yet");
    },
  });

  pi.registerTool({
    name: "sakur4_status",
    label: "Sakur4: status",
    description:
      "Report the resolved inference backend, the cache capabilities Sakur4 detected, and counts for each memory store. Worth calling once at the start of a long session.",
    parameters: { type: "object", properties: {}, additionalProperties: false },
    async execute() {
      const daemon = client.recheck();
      if (!daemon) return unavailable("status");
      const result = client.tool("sakur4.status", {});
      if (!result) return unavailable("status");
      return text(
        `daemon:  ${daemon.command}  (found via ${daemon.how})\n` +
          `store:   ${config.db}\n` +
          `session: ${config.session}\n\n` +
          `backend:      ${result.backend}\n` +
          `capabilities: ${result.capabilities}\n` +
          `coherence:    ${result.cache_coherence}\n` +
          `tokenizer:    ${result.tokenizer}\n` +
          `embedder:     ${result.embedder}\n\n` +
          `episodes ${result.episodes} · facts ${result.symbolic_facts} · ` +
          `atlas ${result.atlas_entries} · stale ${result.stale_entries} · ` +
          `anchors ${result.anchors} · repo files ${result.repo_files}`,
      );
    },
  });

  // -------------------------------------------------------------------------
  // Commands
  // -------------------------------------------------------------------------

  pi.registerCommand("sakur4", {
    description: "Sakur4 memory: status, receipt, recall, pin, index, fold",
    getArgumentCompletions: (prefix: string) => {
      const subcommands = ["status", "receipt", "recall", "pin", "index", "map", "staleness", "dream"];
      return subcommands
        .filter((s) => s.startsWith(prefix))
        .map((s) => ({ value: s, label: s }));
    },
    handler: async (args: string, ctx: ExtensionCommandContext) => {
      const [sub, ...rest] = args.trim().split(/\s+/).filter(Boolean);
      const tail = rest.join(" ");

      if (!sub || sub === "status") {
        const daemon = client.recheck();
        if (!daemon) {
          ctx.ui.notify("Sakur4: no daemon found. Try `cargo install sakur4d`.", "warning");
          return;
        }
        const result = client.tool("sakur4.status", {});
        if (!result) {
          ctx.ui.notify("Sakur4: the daemon did not answer.", "error");
          return;
        }
        ctx.ui.notify(
          `Sakur4 ${result.backend} · ${result.episodes} episodes · ` +
            `${result.anchors} anchors · ${result.symbolic_facts} facts · ` +
            `${result.stale_entries} stale`,
          "info",
        );
        return;
      }

      if (sub === "receipt") {
        const result = client.run(["receipt", config.session]);
        ctx.ui.notify(result?.stdout.trim() ?? "Sakur4: no receipt available", "info");
        return;
      }

      if (sub === "recall") {
        if (!tail) {
          ctx.ui.notify("Usage: /sakur4 recall <query>", "warning");
          return;
        }
        const result = client.run(["recall", tail, "--k", "5", "--session", config.session]);
        ctx.ui.notify(result?.stdout.trim() ?? "Sakur4: nothing found", "info");
        return;
      }

      if (sub === "pin") {
        if (!tail) {
          // Read the last user message rather than prompting, because that is
          // almost always what the user wants pinned.
          ctx.ui.notify(
            "Usage: /sakur4 pin <text>   (or ask the agent to pin something it heard)",
            "warning",
          );
          return;
        }
        const result = client.run(["pin", tail, "--kind", "task_contract", "--session", config.session]);
        ctx.ui.notify(result?.stdout.trim() ?? "Sakur4: could not pin", result ? "info" : "error");
        return;
      }

      if (sub === "index") {
        const root = tail || config.projectRoot;
        ctx.ui.notify(`Sakur4: indexing ${root}…`, "info");
        const result = client.run(["index", root]);
        ctx.ui.notify(result?.stdout.trim() ?? "Sakur4: indexing failed", result ? "info" : "error");
        return;
      }

      if (sub === "map") {
        const budget = tail || "2000";
        const result = client.run(["repo-map", "--budget", budget]);
        ctx.ui.notify(result?.stdout.trim() ?? "Sakur4: no map", "info");
        return;
      }

      if (sub === "staleness") {
        const result = client.tool("memory.staleness", {});
        if (!result) {
          ctx.ui.notify("Sakur4: could not read staleness", "error");
          return;
        }
        const rate = ((result.stale_rate as number) * 100).toFixed(0);
        ctx.ui.notify(
          `Sakur4: ${result.stale} of ${result.total} stored summaries no longer match their source (${rate}%)`,
          result.stale ? "warning" : "info",
        );
        return;
      }

      if (sub === "dream") {
        ctx.ui.notify("Sakur4: running a memory-maintenance pass…", "info");
        const result = client.tool("sakur4.dream", { force: true });
        ctx.ui.notify(result?.summary ? String(result.summary) : "Sakur4: pass finished", "info");
        return;
      }

      ctx.ui.notify(`Sakur4: unknown subcommand '${sub}'`, "warning");
    },
  });

  // -------------------------------------------------------------------------
  // Lifecycle
  // -------------------------------------------------------------------------

  pi.on("session_start", async (_event, ctx) => {
    turns = 0;
    preambleSent = false;
    lastRetrievalTokens = 0;
    client.ensureStoreDir();

    // Probe once at session start so a missing daemon is reported before the user
    // has spent ten turns wondering why nothing is being remembered.
    const daemon = client.recheck();
    if (!daemon) {
      client.warnOnce(ctx);
      return;
    }
    const status = client.tool("sakur4.status", {});
    if (status) {
      ctx.ui.setStatus(
        "sakur4",
        `sakur4: ${status.episodes} ep · ${status.anchors} anc`,
      );
    }
  });

  /**
   * Inject the working preamble, once.
   *
   * `before_agent_start` fires per prompt, so without the guard the preamble would
   * be re-sent every turn — costing tokens and, worse, reading as a new instruction
   * each time. The returned `message` is persisted to the session, so it survives
   * as context rather than needing to be repeated.
   */
  pi.on("before_agent_start", async (_event, ctx) => {
    if (preambleSent) return;
    if (!client.available()) return;
    preambleSent = true;

    // Re-check on every turn, not just the first: a user who installs sakur4d
    // mid-session should not have to restart.
    ctx.ui.setStatus("sakur4", "sakur4: active");
    return {
      message: {
        customType: "sakur4-preamble",
        content: PREAMBLE,
        display: false,
      },
    };
  });

  /**
   * Retrieve relevant memory for the prompt and prepend it.
   *
   * # Why this is bounded and why it reports its own cost
   *
   * Retrieval that runs every turn is a context tax, and an unbounded one is how a
   * memory layer makes a session worse. Two guards: the daemon's `--k` limits the
   * result count, and the injected block is capped here. The block's own token cost
   * is recorded so the usage reported on the next turn can exclude it — otherwise
   * Sakur4's footprint would be attributed to the model and the receipt would lie.
   */
  pi.on("context", async (event, ctx) => {
    if (!config.retrieve || !client.available()) return;

    const prompt = lastUserText(event.messages);
    if (!prompt || prompt.length < 12) return;

    const result = client.tool("memory.recall", {
      query: prompt.slice(0, 600),
      k: 4,
      session_id: config.session,
    });
    const rendered = typeof result?.rendered === "string" ? result.rendered.trim() : "";
    if (!rendered) return;

    const capped =
      rendered.length > config.recallBudget * 4
        ? `${rendered.slice(0, config.recallBudget * 4)}\n…[truncated]`
        : rendered;

    lastRetrievalTokens = Math.ceil(capped.length / 4);
    ctx.ui.setStatus("sakur4", `sakur4: +${lastRetrievalTokens}t recalled`);

    return {
      messages: [
        ...event.messages,
        {
          role: "user" as const,
          content: [
            {
              type: "text" as const,
              text: `<sakur4-memory tokens="${lastRetrievalTokens}">\n${capped}\n</sakur4-memory>`,
            },
          ],
          timestamp: Date.now(),
        },
      ],
    };
  });

  /**
   * Report the provider's token accounting so prompt-cache behaviour is measured
   * every turn rather than only when someone asks.
   *
   * This is the hook that makes the cloud half of Sakur4 useful automatically. The
   * provider already told us how many prompt tokens came from its cache; without
   * forwarding that, Sakur4 can only report what a local llama.cpp slot would have
   * done, which is nothing.
   */
  pi.on("message_end", async (event, _ctx) => {
    if (!config.reportUsage) return;
    if (event.message.role !== "assistant") return;
    if (!client.available()) return;

    const usage: any = (event.message as any).usage;
    if (!usage) return;
    const promptTokens = numberOr(usage.inputTokens ?? usage.input ?? usage.promptTokens, 0);
    if (promptTokens <= 0) return;

    // Field names differ by provider; normalise whichever is present.
    const cacheRead = firstNumber(
      usage.cacheReadTokens,
      usage.cache_read_input_tokens,
      usage.cacheReadInputTokens,
      usage.cachedTokens,
      usage.promptCacheHitTokens,
      usage.prompt_tokens_details?.cached_tokens,
    );
    const cacheWrite = firstNumber(
      usage.cacheWriteTokens,
      usage.cache_creation_input_tokens,
      usage.cacheWriteInputTokens,
      usage.prompt_tokens_details?.cache_creation_tokens,
    );

    const payload: Record<string, unknown> = {
      prompt_tokens: promptTokens,
      completion_tokens: numberOr(usage.outputTokens ?? usage.output ?? usage.completionTokens, 0),
      session_id: config.session,
    };
    // Absent stays absent. Sending zero would assert a cache miss the provider
    // never reported, and Sakur4 says so rather than blaming a cache it cannot see.
    if (cacheRead !== undefined) payload.cache_read_tokens = cacheRead;
    if (cacheWrite !== undefined) payload.cache_write_tokens = cacheWrite;
    if (usage.provider) payload.provider = String(usage.provider);
    if (usage.model) payload.model = String(usage.model);

    const verdict = client.tool("context.record_usage", payload);
    if (!verdict) return;

    // Surface only what needs acting on. A per-turn "cache fine" notification is
    // noise; a broken prefix is money.
    if (verdict.regression === true) {
      _ctx?.ui?.notify?.(
        `Sakur4: this turn was billed for history that had already been paid for — ` +
          `${verdict.detail ?? "the cached prefix shrank"}`,
        "warning",
      );
    }
  });

  /**
   * Take over compaction.
   *
   * # The claim being tested
   *
   * A default compaction summarises, and a summary produces a prompt with no
   * prefix in common with the previous one — so the provider's prompt cache is
   * invalidated from the rewrite point onward and the next request pays full price
   * for history it had already paid for. The harness's own documentation names this
   * as the strongest argument against per-turn compaction.
   *
   * Sakur4's eviction engine is built to choose a boundary that preserves a prefix.
   * This hook asks it for that plan, applies it, and builds the summary from what
   * it decided to keep plus the pinned anchors. Report-and-apply rather than
   * summarise-and-hope.
   *
   * # Why it falls back
   *
   * If the plan is empty, the daemon is unavailable, or the plan would not actually
   * reduce the context, returning nothing lets OMP use its own compaction. A
   * bespoke compaction that is worse than the default is not an improvement.
   */
  pi.on("session_before_compact", async (event, ctx) => {
    if (!config.ownCompaction) return;
    const daemon = client.available();
    if (!daemon) return;

    const { preparation, signal } = event as any;
    const tokensBefore = numberOr(preparation?.tokensBefore, 0);
    const firstKeptEntryId = preparation?.firstKeptEntryId;

    const plan = client.tool("context.plan_eviction", {
      session_id: config.session,
      slot_id: "0",
      apply: true,
    });
    if (!plan || signal?.aborted) return;

    const reclaimed = numberOr(plan.planned_savings, 0);
    const applied = plan.applied === true || numberOr(plan.applied, 0) > 0;
    // Nothing was actually evicted: let the host do its normal compaction.
    if (!applied && reclaimed === 0) return;

    const anchors = client.tool("memory.recall", {
      query: "pinned constraints and task requirements",
      k: 6,
      session_id: config.session,
    });
    const anchorText = typeof anchors?.rendered === "string" ? anchors.rendered.trim() : "";

    const cacheLine =
      typeof plan.cache_status === "string"
        ? `Cache verdict for the new boundary: ${plan.cache_status}. ${plan.cache_reason ?? ""}`
        : "Cache verdict unavailable.";

    const summary = [
      "# Context compaction (Sakur4)",
      "",
      "The conversation history was evicted by Sakur4's Graduated Eviction Engine",
      "rather than summarised. What follows is what the engine kept and why.",
      "",
      "## Eviction decision",
      "",
      `- Episodes evicted: ${numberOr(plan.updates && (plan.updates as any[]).length, 0)}`,
      `- Tokens reclaimed: ${reclaimed}`,
      `- Context before: ${tokensBefore}`,
      "",
      plan.summary ? `## Engine's own account\n\n${plan.summary}\n` : "",
      "## Cache impact",
      "",
      cacheLine,
      "",
      anchorText
        ? `## Pinned constraints (survive every compaction, verbatim)\n\n${anchorText}\n`
        : "",
      "## Standing instructions",
      "",
      "Continue the work in progress. Earlier steps that left this context are still",
      "retrievable with sakur4_recall, which searches the same store this compaction",
      "acted on — so nothing evicted here is lost, only unwindowed.",
    ]
      .filter(Boolean)
      .join("\n");

    if (reclaimed > 0) {
      ctx.ui.notify(
        `Sakur4 compaction: reclaimed ${reclaimed} tokens · ${plan.cache_status ?? "cache unknown"}`,
        plan.cache_status === "partial-reuse" ? "info" : "warning",
      );
    }

    return {
      compaction: {
        summary,
        firstKeptEntryId,
        tokensBefore,
      },
    };
  });

  /**
   * Contribute the bundled skill, so installing the plugin also installs the
   * model-facing instructions for *when* to use these tools.
   *
   * Tools without guidance get under-used: a model with `sakur4_pin` available and
   * no instruction to reach for it will not pin, and an unpinned requirement is one
   * compaction away from being lost. The preamble covers the session's own habits;
   * the skill carries the detail — every subcommand, the provider field-name table,
   * worked examples — at no context cost until the model loads it.
   *
   * The path is resolved relative to the plugin's own directory, and simply omitted
   * when `skills/` is absent: a copied install without it still works, just with
   * less guidance.
   */
  pi.on("resources_discover", async () => {
    const here = dirname(fileURLToPath(import.meta.url));
    const candidates = [
      // Installed as a package directory that carries its own `skills/`.
      join(here, "skills"),
      // Installed straight from `integrations/omp-plugin/`, beside the
      // repository's top-level `skills/`.
      join(here, "..", "..", "skills"),
    ];
    for (const candidate of candidates) {
      if (existsSync(join(candidate, "sakur4", "SKILL.md"))) {
        diagnose("resources_discover: offering skill", { path: candidate });
        return { skillPaths: [candidate] };
      }
    }
    diagnose("resources_discover: no bundled skill found");
    return;
  });

  /** Flush state and report what the session cost. */
  pi.on("session_shutdown", async (_event, ctx) => {
    if (!client.available()) return;
    const stats = client.tool("memory.staleness", {});
    if (stats && numberOr(stats.stale, 0) > 0) {
      // Worth saying at the end, because the next session inherits it and the fix
      // is one command.
      ctx.ui.notify(
        `Sakur4: ${stats.stale} stored summar${numberOr(stats.stale, 0) === 1 ? "y" : "ies"} ` +
          `no longer match their source. Run /sakur4 dream to regenerate them.`,
        "warning",
      );
    }
  });

  // -------------------------------------------------------------------------
  // Helpers
  // -------------------------------------------------------------------------

  function numberOr(value: unknown, fallback: number): number {
    const n = typeof value === "number" ? value : Number.parseInt(String(value ?? ""), 10);
    return Number.isFinite(n) ? n : fallback;
  }

  function firstNumber(...values: unknown[]): number | undefined {
    for (const value of values) {
      if (value === undefined || value === null) continue;
      const n = typeof value === "number" ? value : Number.parseInt(String(value), 10);
      if (Number.isFinite(n)) return n;
    }
    return undefined;
  }

  /** The most recent user message, which is what retrieval should be keyed on. */
  function lastUserText(messages: any[]): string | undefined {
    for (let i = messages.length - 1; i >= 0; i--) {
      const message = messages[i];
      if (message?.role !== "user") continue;
      const content = message.content;
      if (typeof content === "string") return content;
      if (Array.isArray(content)) {
        const joined = content
          .filter((c: any) => c?.type === "text" && typeof c.text === "string")
          .map((c: any) => c.text)
          .join("\n");
        if (joined) return joined;
      }
    }
    return undefined;
  }

  void turns;
}
