#!/usr/bin/env node
/**
 * One entry point for every verification this project has.
 *
 * # The problem this solves
 *
 * Verification had grown to fourteen artefacts in five kinds, spread across three
 * languages and three harnesses, with no way to answer "does this all still work?"
 * without remembering which script needed which daemon on which port. Worse, several of
 * them need something this machine may not have — a live llama.cpp, OMP, OpenSSL
 * development files — and a check that cannot run looks exactly like a check that passed
 * if nobody says otherwise.
 *
 * So: one command, one summary, and **`skip` is a first-class outcome that is reported
 * separately from `pass`**. A run that skipped its live-server checks is not a green run,
 * and this refuses to call it one.
 *
 * # Usage
 *
 *   node verify.mjs                          # everything this machine can run
 *   node verify.mjs --upstream http://host:8080  # include the live-server checks
 *   node verify.mjs --quick                  # skip the slow benchmarks
 *   node verify.mjs --only rust,proxy        # a subset, by id or group
 *   node verify.mjs --require-all            # fail if anything was skipped
 *   node verify.mjs --list                   # show what would run
 *
 * # What each kind needs
 *
 * | Group | Needs | Runs in CI |
 * |---|---|---|
 * | `rust` | nothing | yes, on three platforms |
 * | `encryption` | OpenSSL development files | yes, on Linux |
 * | `hermes` | Python and a sakur4d binary | yes, with a stubbed Hermes |
 * | `bench` | a repository to index | partly |
 * | `live` | a real llama.cpp server | no — no server in CI |
 * | `harness` | OMP, or the Hermes CLI | no — not installed in CI |
 *
 * The distinction between "CI covers this" and "only verifiable here" is the reason the
 * summary prints a group per line rather than one number.
 */

import { spawn, spawnSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = dirname(fileURLToPath(import.meta.url));
const EXE = process.platform === "win32" ? "sakur4d.exe" : "sakur4d";

// A mislocated root is the one failure that makes every later check meaningless: `cargo`
// reports "could not find Cargo.toml" and the script cheerfully reports six failures that
// are really one. Check it once, up front, and name the actual problem.
if (!existsSync(join(ROOT, "Cargo.toml"))) {
  process.stderr.write(
    `verify.mjs must sit at the workspace root; ${ROOT} has no Cargo.toml.\n`,
  );
  process.exit(2);
}

// ===========================================================================
// Arguments
// ===========================================================================

const argv = process.argv.slice(2);
const has = (flag) => argv.includes(`--${flag}`);
const value = (flag, fallback = null) => {
  const index = argv.indexOf(`--${flag}`);
  return index >= 0 && argv[index + 1] && !argv[index + 1].startsWith("--")
    ? argv[index + 1]
    : fallback;
};

const UPSTREAM = value("upstream", process.env.SAKUR4_UPSTREAM ?? null);
const ONLY = (value("only", "") || "")
  .split(",")
  .map((s) => s.trim())
  .filter(Boolean);
const QUICK = has("quick");
const REQUIRE_ALL = has("require-all");
const JSON_OUT = value("json", null);

// ===========================================================================
// Running things
// ===========================================================================

/** A check's outcome. `skip` is deliberately not `pass`. */
const PASS = "pass";
const FAIL = "fail";
const SKIP = "skip";

const results = [];
// Every group and id that actually ran, so a filter matching nothing can be reported rather
// than silently succeeding. See the check in `main`.
// # The full catalog, declared rather than discovered
//
// Recording only what *ran* would make the "matched no check" message print an empty list of
// known names exactly when it is most needed — when a filter excluded everything, or when a
// user mistyped. This is every group and id the script can produce, kept next to the code that
// produces them so a drift is visible.
const CATALOG = {
  rust: ["cargo fmt", "cargo clippy", "cargo test", "doctests", "cargo doc"],
  encryption: ["encryption at rest (FR-20)"],
  hermes: ["context engine (FR-16)"],
  bench: ["A/B (scripted)", "NFR-2 recall at scale"],
  live: [
    "llama.cpp prefix behaviour",
    "compaction case",
    "proxy rewrites an over-window transcript",
    "proxy trims a single large request",
  ],
  harness: [
    "OMP extension",
    "generated configs name an absolute store",
    "OMP extension command forms",
    "integration tool names exist",
    "snapshot and restore",
    "usage accounting round trip",
    "Hermes plugin",
  ],
};
const KNOWN_NAMES = [...new Set([...Object.keys(CATALOG), ...Object.values(CATALOG).flat()])].sort();
const recordedGroups = new Set();
const recordedIds = new Set();

/**
 * Record one check's outcome.
 *
 * `id` is what `--only` accepts and what `CATALOG` lists; `label` is what a person reads. They
 * were the same string until the catalog guard pointed out that `--only fmt` could never match a
 * check whose id was `cargo fmt` — the id is a selector, not a sentence.
 */
function record(group, id, status, note) {
  results.push({ group, id, status, note });
  recordedGroups.add(group);
  recordedIds.add(id);
  const mark = { [PASS]: "PASS", [FAIL]: "FAIL", [SKIP]: "SKIP" }[status];
  const colour = { [PASS]: "\x1b[32m", [FAIL]: "\x1b[31m", [SKIP]: "\x1b[33m" }[status];
  process.stdout.write(
    `  ${colour}${mark}\x1b[0m  ${id}${note ? `  \x1b[2m— ${note}\x1b[0m` : ""}\n`,
  );
}

/**
 * Run a command, streaming nothing, returning `{ ok, output }`.
 *
 * Not `stdio: inherit`, because a failing check's output is worth showing and a passing
 * one's is not — the summary would be buried under several thousand lines of test output.
 */
function run(command, args, { cwd = ROOT, env = {}, timeout = 900_000 } = {}) {
  const result = spawnSync(command, args, {
    cwd,
    encoding: "utf8",
    shell: false,
    timeout,
    maxBuffer: 64 * 1024 * 1024,
    env: { ...process.env, ...env },
  });
  const output = `${result.stdout ?? ""}${result.stderr ?? ""}`;
  return { ok: result.status === 0, output, status: result.status, error: result.error };
}

/**
 * Does this command exist and answer?
 *
 * # Why "produced output" counts, and why transient failure is retried
 *
 * The first version required `status === 0` within 20 seconds. That is wrong twice over.
 *
 * A tool can answer correctly and still exit non-zero. `hermes` was doing exactly that from a
 * PowerShell wrapper — printing its version and returning a failure code — so an installed
 * harness was reported as `hermes not on PATH`, which is the opposite of the truth and sends a
 * reader looking for an installation problem.
 *
 * And a probe that starts a whole application can exceed a fixed timeout on a busy machine.
 * This one was reached only after four heavy checks had run, the *last* of which invokes a
 * harness that talks to a model. The same command answered in a second when run alone and timed
 * out in the full pass, so the check silently became a skip depending on what ran before it —
 * the worst kind of flake, because it looks like an environment fact.
 *
 * So: an answer is evidence the command exists and works, whichever way it exited; and a first
 * attempt that fails outright is retried once with more time, because the alternative is a
 * verifier whose result depends on machine load.
 */
function available(command, args = ["--version"]) {
  const attempt = (timeout) =>
    spawnSync(command, args, { encoding: "utf8", shell: false, timeout, maxBuffer: 8 * 1024 * 1024 });

  let result = attempt(20_000);
  if (result.error || result.status !== 0) {
    result = attempt(60_000);
  }

  const producedOutput = `${result.stdout ?? ""}${result.stderr ?? ""}`.trim().length > 0;
  // `status === null` with no error means the process was killed by a signal, which for a
  // version probe means it never got to answer.
  return producedOutput || (!result.error && result.status === 0);
}

function daemonBinary() {
  if (process.env.SAKUR4_BIN && existsSync(process.env.SAKUR4_BIN)) return process.env.SAKUR4_BIN;
  const candidates = [
    join(ROOT, "target", "release", EXE),
    join(ROOT, "target", "debug", EXE),
    join(process.env.USERPROFILE ?? process.env.HOME ?? "", ".cargo", "bin", EXE),
  ];
  return candidates.find(existsSync) ?? null;
}

function wanted(id, group) {
  if (ONLY.length === 0) return true;
  return ONLY.includes(id) || ONLY.includes(group);
}

function lastLines(output, n = 6) {
  return output.trim().split("\n").slice(-n).join("\n");
}

// ===========================================================================
// Groups
// ===========================================================================

/** Rust: formatting, lints, tests, docs. No external requirements. */
function rustChecks() {
  if (wanted("fmt", "rust")) {
    const r = run("cargo", ["fmt", "--all", "--", "--check"]);
    record("rust", "cargo fmt", r.ok ? PASS : FAIL, r.ok ? "" : lastLines(r.output, 3));
  }

  if (wanted("clippy", "rust")) {
    // Default features: `--all-features` would pull in encryption, which needs OpenSSL
    // development files this machine may not have. The encryption group covers that.
    const r = run("cargo", ["clippy", "--workspace", "--all-targets", "--", "-D", "warnings"]);
    record("rust", "cargo clippy", r.ok ? PASS : FAIL, r.ok ? "" : lastLines(r.output, 6));
  }

  if (wanted("tests", "rust")) {
    const r = run("cargo", ["test", "--workspace", "--all-targets"]);
    const summary = (r.output.match(/test result: ok\. (\d+) passed/g) ?? [])
      .map((m) => Number(m.match(/(\d+) passed/)[1]))
      .reduce((a, b) => a + b, 0);
    record("rust", "cargo test", r.ok ? PASS : FAIL, r.ok ? `${summary} tests` : lastLines(r.output, 8));
  }

  if (wanted("doctests", "rust")) {
    const r = run("cargo", ["test", "--workspace", "--doc"]);
    record("rust", "doctests", r.ok ? PASS : FAIL, r.ok ? "" : lastLines(r.output, 4));
  }

  if (wanted("docs", "rust")) {
    const r = run("cargo", ["doc", "--workspace", "--no-deps"], {
      env: { RUSTDOCFLAGS: "-D warnings" },
    });
    record("rust", "cargo doc", r.ok ? PASS : FAIL, r.ok ? "" : lastLines(r.output, 4));
  }
}

/** Encryption (FR-20): an opt-in feature whose tests cannot run without OpenSSL. */
function encryptionChecks() {
  if (!wanted("encryption", "encryption")) return;

  const probe = run("cargo", ["check", "-p", "sakur4-core", "--features", "encryption"], {
    timeout: 600_000,
  });
  if (!probe.ok && /OPENSSL_DIR|openssl|sqlcipher/i.test(probe.output)) {
    record(
      "encryption",
      "encryption at rest (FR-20)",
      SKIP,
      "SQLCipher needs OpenSSL development files; see crates/sakur4-core/Cargo.toml",
    );
    return;
  }
  if (!probe.ok) {
    record("encryption", "encryption at rest (FR-20)", FAIL, lastLines(probe.output, 6));
    return;
  }

  const r = run("cargo", [
    "test",
    "-p",
    "sakur4-core",
    "--features",
    "encryption",
    "--test",
    "encryption_at_rest",
  ]);
  record(
    "encryption",
    "encryption at rest (FR-20)",
    r.ok ? PASS : FAIL,
    r.ok ? "acceptance criterion: unreadable without the key" : lastLines(r.output, 6),
  );
}

/** The Hermes ContextEngine (FR-16): Python, and needs a daemon to talk to. */
async function hermesChecks() {
  if (!wanted("hermes", "hermes")) return;

  const plugin = join(ROOT, "integrations", "hermes-plugin");
  const verify = join(plugin, "verify_engine.py");
  if (!existsSync(verify)) {
    record("hermes", "context engine (FR-16)", SKIP, "verify_engine.py not found");
    return;
  }
  if (!available("python", ["--version"])) {
    record("hermes", "context engine (FR-16)", SKIP, "python not on PATH");
    return;
  }
  const binary = daemonBinary();
  if (!binary) {
    record("hermes", "context engine (FR-16)", SKIP, "no sakur4d binary built");
    return;
  }

  // The engine imports `agent.context_engine`, which only exists inside a Hermes install.
  // CI stubs it; locally the real one is used when Hermes is present, because testing
  // against the real interface is strictly better than testing against a stub of it.
  const hermesHome = join(process.env.LOCALAPPDATA ?? join(process.env.HOME ?? "", ".local", "share"), "hermes-agent");
  const agentDir = existsSync(join(hermesHome, "agent", "context_engine.py"))
    ? hermesHome
    : makeHermesStub();

  // # Start the daemon and wait for it to answer
  //
  // Not via `curl`: on Windows `-o /dev/null` is not a path curl can open, so it exits 23
  // and the probe silently "succeeds" by failing. Not by sleeping a fixed amount either,
  // which would fail a slow machine for being slow rather than for being wrong. A real
  // HTTP request with a deadline is both portable and honest.
  const workdir = mkdtempSync(join(tmpdir(), "sakur4-hermes-"));
  const store = join(workdir, "engine.db");
  const port = 8901 + (process.pid % 80);
  const child = spawnDaemon(binary, [
    "--db", store,
    "--backend", "embedded",
    "--context-window", "8192",
    "serve",
    "--transport", "http",
    "--bind", `127.0.0.1:${port}`,
    "--no-dream",
  ]);

  const up = await waitForPort(port, 30_000);

  const r = up
    ? run("python", [verify], {
        env: {
          SAKUR4_URL: `http://127.0.0.1:${port}`,
          HERMES_AGENT_DIR: agentDir,
          SAKUR4_HERMES_PLUGIN: plugin,
        },
      })
    : { ok: false, output: `the daemon did not answer on port ${port} within 30s` };

  const passed = r.ok && /all contracts pass/.test(r.output);
  record(
    "hermes",
    "context engine (FR-16)",
    passed ? PASS : FAIL,
    passed
      ? `${(r.output.match(/\[PASS\]/g) ?? []).length} contracts against a live daemon`
      : lastLines(r.output, 8),
  );

  try {
    child?.kill();
  } catch {
    /* already gone */
  }
  cleanupDaemon(port, workdir);
}

/** Spawn a daemon, keeping the handle so it can be killed when the check is done. */
function spawnDaemon(binary, args) {
  return spawn(binary, args, { stdio: "ignore" });
}

/** Wait until something answers on a local port, or give up. */
async function waitForPort(port, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    try {
      const response = await fetch(`http://127.0.0.1:${port}/`, {
        method: "POST",
        headers: { "content-type": "application/json", accept: "application/json" },
        body: "{}",
        signal: AbortSignal.timeout(2000),
      });
      // Any HTTP answer means something is listening; a 4xx is the daemon rejecting an
      // empty body, which is a perfectly good sign of life.
      if (response.status > 0) return true;
    } catch {
      /* not up yet */
    }
    await new Promise((resolve) => setTimeout(resolve, 300));
  }
  return false;
}

/** A stub of Hermes' ABC, so the engine can be imported where Hermes is not installed. */
function makeHermesStub() {
  const dir = mkdtempSync(join(tmpdir(), "sakur4-hermes-stub-"));
  const agent = join(dir, "agent");
  mkdirSync(agent, { recursive: true });
  writeFileSync(join(agent, "__init__.py"), "");
  writeFileSync(
    join(agent, "context_engine.py"),
    [
      "from abc import ABC, abstractmethod",
      "from typing import Any, Dict, List, Optional",
      "",
      "",
      "class ContextEngine(ABC):",
      "    last_prompt_tokens: int = 0",
      "    last_completion_tokens: int = 0",
      "    last_total_tokens: int = 0",
      "    threshold_tokens: int = 0",
      "    context_length: int = 0",
      "    compression_count: int = 0",
      "    threshold_percent: float = 0.75",
      "    protect_first_n: int = 3",
      "    protect_last_n: int = 6",
      "    emit_automatic_compaction_status: bool = True",
      "",
      "    @property",
      "    @abstractmethod",
      "    def name(self) -> str: ...",
      "",
      "    @abstractmethod",
      "    def update_from_response(self, usage: Dict[str, Any]) -> None: ...",
      "",
      "    @abstractmethod",
      "    def should_compress(self, prompt_tokens: Optional[int] = None) -> bool: ...",
      "",
      "    @abstractmethod",
      "    def compress(self, messages: List[Dict[str, Any]], current_tokens: Optional[int] = None,",
      "                 focus_topic: Optional[str] = None, force: bool = False,",
      "                 memory_context: str = \"\") -> List[Dict[str, Any]]: ...",
      "",
      "    def on_session_start(self, session_id: str, **kwargs: Any) -> None: ...",
      "    def on_session_reset(self) -> None: ...",
      "    def update_model(self, model: Any = None, **kwargs: Any) -> None: ...",
      "",
    ].join("\n"),
  );
  return dir;
}

function cleanupDaemon(port, workdir) {
  // Best-effort: the daemon is detached and holds a port, so leaving it would make the
  // next run fail for a reason that has nothing to do with the code.
  if (process.platform === "win32") {
    spawnSync("powershell", [
      "-NoProfile",
      "-Command",
      `Get-NetTCPConnection -LocalPort ${port} -State Listen -ErrorAction SilentlyContinue | ` +
        `ForEach-Object { Stop-Process -Id $_.OwningProcess -Force -ErrorAction SilentlyContinue }`,
    ], { shell: false });
  } else {
    spawnSync("sh", ["-c", `lsof -ti tcp:${port} | xargs -r kill -9`], { shell: false });
  }
  try {
    rmSync(workdir, { recursive: true, force: true });
  } catch {
    /* the OS will clean up tmp */
  }
}

/** Benchmarks: the scripted A/B, and the live-model probes when a server is present. */
function benchChecks() {
  if (wanted("ab", "bench")) {
    const binary = daemonBinary();
    if (!binary) {
      record("bench", "A/B (scripted)", SKIP, "no sakur4d binary built");
    } else {
      const workdir = mkdtempSync(join(tmpdir(), "sakur4-ab-"));
      // Sized so both arms actually compact. An earlier configuration ran 60 turns at 600
      // tokens each, which never crossed the budget — and the benchmark, correctly, refused
      // to report rather than comparing two unbounded transcripts. A verification run that
      // measures nothing is not a passing run, so these parameters have to create the
      // pressure the check exists to observe.
      const r = run(process.execPath, [
        join(ROOT, "docs", "bench", "ab.mjs"),
        "--repo", ROOT,
        "--turns", "150",
        "--tokens-per-turn", "1500",
        "--bin", binary,
        "--workdir", workdir,
      ], { timeout: 1_200_000 });
      // The benchmark refuses to report when neither arm compacted, which is a failure of
      // the run rather than of the code — say which.
      const compacted = /compactions\s+\d+\s+\d+/.test(r.output) && !/REFUSING TO REPORT/.test(r.output);
      record(
        "bench",
        "A/B (scripted)",
        r.ok && compacted ? PASS : FAIL,
        r.ok && compacted ? "both arms compacted" : lastLines(r.output, 6),
      );
      try {
        rmSync(workdir, { recursive: true, force: true });
      } catch {
        /* ignore */
      }
    }
  }

  if (wanted("nfr", "bench") && !QUICK) {
    const binary = daemonBinary();
    const script = join(ROOT, "docs", "verification", "nfr2-recall.mjs");
    if (!binary || !existsSync(script)) {
      record("bench", "NFR-2 recall at scale", SKIP, "needs a built daemon");
    } else {
      const verdictFile = join(tmpdir(), `sakur4-nfr2-${process.pid}.json`);
      const r = run(process.execPath, [script, "--n", "20000", "--reps", "6", "--bin", binary], {
        timeout: 1_200_000,
        env: { SAKUR4_VERDICT_JSON: verdictFile },
      });
      // Read the verdict from a field, not from prose: deciding pass or fail by matching
      // text in a human-readable report is how a passing 21 ms run gets reported as a
      // failure, which is exactly what happened before this.
      let verdict = null;
      try {
        verdict = JSON.parse(readFileSync(verdictFile, "utf8"));
      } catch {
        /* the script did not get far enough to write one */
      }
      try {
        rmSync(verdictFile, { force: true });
      } catch {
        /* ignore */
      }
      record(
        "bench",
        "NFR-2 recall at scale",
        verdict?.verdict === "PASS" ? PASS : FAIL,
        verdict
          ? `worst p95 ${verdict.worstP95Ms.toFixed(1)} ms over ${verdict.entries.toLocaleString()} entries (target < ${verdict.target} ms)`
          : lastLines(r.output, 6),
      );
    }
  }
}

/** Live checks: everything that needs a real llama.cpp. */
async function liveChecks() {
  // # Why `--only` suppresses the skip records, not just the checks
  //
  // A caller who passes `--only rust,bench` has said which groups they care about. If the
  // live checks still recorded "skipped — pass --upstream", then `--require-all` would fail
  // on a deliberately narrow run, and the only way to combine the two would be to also pass
  // `--upstream` — defeating the point of narrowing. Asking for nothing in particular is
  // different: then the skipped live checks are real information, because the run was meant
  // to be comprehensive.
  if (ONLY.length > 0 && !ONLY.includes("live")) return;

  if (!UPSTREAM) {
    record("live", "llama.cpp prefix behaviour", SKIP, "pass --upstream to enable");
    record("live", "compaction case", SKIP, "pass --upstream to enable");
    return;
  }
  if (!wanted("live", "live")) return;

  const script = join(ROOT, "docs", "verification", "llamacpp-prefix.mjs");
  // # Read the verdict field, not the prose
  //
  // `llamacpp-prefix.mjs` decides with `b.ms < a.ms * 0.5` — a wall-clock threshold — so under
  // load a genuine 4x speedup can measure below 2x. Parsing "automatic prefix reuse WORKS" out
  // of its output turned that wobble into a failed check: one full run reported `live 1 failed`,
  // and three subsequent runs of the identical command were green, with nothing in the failure
  // to say which check had moved or why.
  //
  // The script now writes the verdict *and* the measurements to a field. Same lesson as the
  // benchmark's own verdict file, in a different script, three years of rounds apart.
  const verdictFile = join(tmpdir(), `sakur4-prefix-${process.pid}.json`);
  const r = run(process.execPath, [script, "--base", UPSTREAM, "--tokens", "1500", "--long", "3000"], {
    timeout: 1_200_000,
    env: { SAKUR4_VERDICT_JSON: verdictFile },
  });
  let header = null;
  try {
    header = JSON.parse(readFileSync(verdictFile, "utf8"));
  } catch {
    /* the script did not get far enough to write one */
  }
  try {
    rmSync(verdictFile, { force: true });
  } catch {
    /* ignore */
  }
  record(
    "live",
    "llama.cpp prefix behaviour",
    header?.verdict === "PASS" ? PASS : FAIL,
    header
      ? `warm ${header.warmMs} ms against ${header.coldMs} ms cold (${Math.round((1 - header.ratio) * 100)}% faster)`
      : lastLines(r.output, 6),
  );

  const rule = join(ROOT, "docs", "verification", "reuse-rule.mjs");
  if (existsSync(rule)) {
    // # Asserted on the numbers, from the table the script actually prints
    //
    // The previous version regex-matched prose: `/A 2,219-token preserved prefix|preserved
    // prefix[^\n]*reused/`, with a hard-coded token count and a fallback on the string
    // "reused %". Section A prints a *table* — `pad tokens / prompt tokens / cache_n /
    // prompt_n / reused %` — and none of that prose is in it, so the check was matching a
    // string that does not exist and relying on whichever fallback happened to fire.
    //
    // The real invariant is in the table: with the head held fixed and the tail growing,
    // `cache_n` stays just behind `prompt tokens` and the reused share stays high. That is
    // what "a preserved prefix is reused" means, and it is a property of numbers the server
    // reported rather than of wording this script chose.
    const rr = run(process.execPath, [rule, "--base", UPSTREAM], { timeout: 1_200_000 });
    const rows = [];
    for (const line of rr.output.split("\n")) {
      const m = line.match(/^\s+(\d+)\s+(\d+)\s+(\d+)\s+(\d+)\s+(\d+)%/);
      if (m) rows.push({ pad: +m[1], prompt: +m[2], cacheN: +m[3], reusedPct: +m[5] });
    }
    // Section A's rows are the ones with a growing pad; the warm check above them has no pad.
    const sectionA = rows.filter((row) => row.pad > 0);
    const worst = sectionA.length ? Math.min(...sectionA.map((r) => r.reusedPct)) : 0;
    const tracked = sectionA.filter((r) => r.cacheN > 0 && r.cacheN <= r.prompt).length;
    const preserves = sectionA.length >= 5 && worst >= 80 && tracked === sectionA.length;
    record(
      "live",
      "compaction case",
      preserves ? PASS : FAIL,
      preserves
        ? `a preserved prefix is reused: ${sectionA.length} tails, worst ${worst}%`
        : `expected every row's cache_n to track its prompt with high reuse; ` +
          `got ${sectionA.length} rows, worst ${worst}%, ${tracked} tracking — ` +
          lastLines(rr.output, 4),
    );
  }

  // The reverse proxy's rewrite path, driven with a real multi-turn transcript. It is wired in
  // rather than left as a script so the behaviour stays visible instead of becoming a document.
  const rewrite = join(ROOT, "docs", "verification", "proxy-rewrite.mjs");
  const binary = daemonBinary();
  if (existsSync(rewrite) && binary) {
    const rw = run(
      process.execPath,
      [rewrite, "--bin", binary, "--upstream", UPSTREAM],
      { timeout: 1_800_000 },
    );
    const ok = /VERDICT: PASS/.test(rw.output);
    record(
      "live",
      "proxy rewrites an over-window transcript",
      ok ? PASS : FAIL,
      ok ? "10 contracts: trimmed, marked, and proportionate" : lastLines(rw.output, 8),
    );

    // # A single large request is a different path from a growing session
    //
    // The check above sends its transcript in one shot too, but the *growth* test is what
    // distinguishes the two cases: a session that grows a turn at a time lets each plan pick
    // up the tier ladder where the last one left it. For five rounds the single-request case
    // was broken while staged tests passed — the plan advanced 903 episodes, reported zero
    // savings, and the proxy discarded the result and forwarded everything.
    //
    // The assertion here is that retained context tracks the target, which is 30% of the
    // window under `window-first`. A settled value far below it is precisely the failure, and
    // it is invisible to any "did it rewrite" check because nothing is rewritten.
    const growth = join(ROOT, "docs", "verification", "grow-session.mjs");
    if (existsSync(growth)) {
      const port = 8700 + (process.pid % 60);
      const growDir = mkdtempSync(join(tmpdir(), "sakur4-grow-"));
      const daemon = spawnDaemon(binary, [
        "--db", join(growDir, "grow.db"),
        "--backend", UPSTREAM,
        "--context-window", "32768",
        "proxy",
        "--bind", `127.0.0.1:${port}`,
        "--upstream", UPSTREAM,
        "--session", "verify-grow",
      ]);
      let ready = false;
      const deadline = Date.now() + 25_000;
      while (Date.now() < deadline && !ready) {
        try {
          const probe = await fetch(`http://127.0.0.1:${port}/v1/models`, {
            signal: AbortSignal.timeout(1500),
          });
          ready = probe.status > 0;
        } catch {
          await new Promise((r) => setTimeout(r, 250));
        }
      }
      if (!ready) {
        record("live", "proxy trims a single large request", SKIP, "the proxy did not start");
      } else {
        const g = run(
          process.execPath,
          [growth, "--proxy", `http://127.0.0.1:${port}`, "--steps", "600", "--every", "300"],
          { timeout: 1_200_000 },
        );
        const settled = Number((g.output.match(/Settled at (\d+) tokens/) ?? [])[1] ?? 0);
        const target = Math.round(32_768 * 0.3);
        const good = settled >= target * 0.4;
        record(
          "live",
          "proxy trims a single large request",
          good ? PASS : FAIL,
          settled === 0
            ? lastLines(g.output, 6)
            : `settled at ${settled} tokens against a ${target}-token target` +
              (good ? "" : " — far below it, so the trim erases rather than shortens"),
        );
      }
      try {
        daemon?.kill();
      } catch {
        /* already gone */
      }
      // The daemon may still hold the store open, and on Windows an open handle makes the
      // removal fail with EBUSY rather than being deferred. Losing a temp directory is not
      // worth failing a verification run over, so the OS gets to clean up instead.
      try {
        rmSync(growDir, { recursive: true, force: true });
      } catch {
        /* the OS will reclaim the temp directory */
      }
    }
  }
}

/** Harness integrations that need the harness itself. */
function harnessChecks() {
  if (wanted("omp", "harness")) {
    if (!available("omp", ["--version"])) {
      record("harness", "OMP extension", SKIP, "omp not on PATH");
    } else {
      const installed = existsSync(
        join(process.env.USERPROFILE ?? "", ".omp", "plugins", "node_modules", "omp-sakur4", "index.ts"),
      );
      record(
        "harness",
        "OMP extension",
        installed ? PASS : SKIP,
        installed ? "installed and discoverable" : "not installed — run integrations/omp-plugin/install.mjs",
      );
    }
  }

  // A generated config is the first thing a user pastes into a harness, and a relative store
  // path in it lands the memory wherever that harness happens to spawn the process. Checked
  // from an unrelated working directory, because that is the condition that exposes it.
  if (wanted("config-paths", "harness")) {
    const script = join(ROOT, "docs", "verification", "config-paths.mjs");
    const binary = daemonBinary();
    if (!existsSync(script) || !binary) {
      record("harness", "generated configs name an absolute store", SKIP, "needs a built daemon");
    } else {
      const r = run(process.execPath, [script, binary]);
      const ok = r.ok && /every harness config names an absolute store path/.test(r.output);
      record(
        "harness",
        "generated configs name an absolute store",
        ok ? PASS : FAIL,
        ok ? "all five harnesses" : lastLines(r.output, 6),
      );
    }
  }

  // The extension drives the daemon with exact CLI argument shapes. They were written against a
  // documented interface and never exercised outside OMP, so a renamed flag would surface only
  // when a user ran that tool in a session.
  if (wanted("omp-commands", "harness")) {
    const script = join(ROOT, "docs", "verification", "omp-commands.mjs");
    const binary = daemonBinary();
    if (!existsSync(script) || !binary) {
      record("harness", "OMP extension command forms", SKIP, "needs a built daemon");
    } else {
      const r = run(process.execPath, [script, binary]);
      const ok = r.ok && /all \d+ extension command forms succeed/.test(r.output);
      record(
        "harness",
        "OMP extension command forms",
        ok ? PASS : FAIL,
        ok ? "10 CLI shapes the extension builds" : lastLines(r.output, 6),
      );
    }
  }

  // A tool name typo is invisible until that one tool is called: the rest of the integration
  // keeps working, so the plugin looks healthy while a tool is dead.
  if (wanted("tool-names", "harness")) {
    const script = join(ROOT, "docs", "verification", "tool-names.mjs");
    const binary = daemonBinary();
    if (!existsSync(script) || !binary) {
      record("harness", "integration tool names exist", SKIP, "needs a built daemon");
    } else {
      const r = run(process.execPath, [script, binary]);
      const ok = r.ok && /every tool name used by an integration exists/.test(r.output);
      record(
        "harness",
        "integration tool names exist",
        ok ? PASS : FAIL,
        ok ? "OMP 10 + Hermes 4, all in the catalog" : lastLines(r.output, 6),
      );
    }
  }

  // Snapshot / restore (FR-8) had never been called. Both halves matter: the round trip on a
  // backend that supports it, and — on the user's llama.cpp, which returns 501 for save — a
  // refusal clear enough that nobody believes their session is recoverable when it is not.
  if (wanted("snapshot", "harness") && UPSTREAM) {
    const script = join(ROOT, "docs", "verification", "snapshot-roundtrip.mjs");
    const binary = daemonBinary();
    if (!existsSync(script) || !binary) {
      record("harness", "snapshot and restore", SKIP, "needs a built daemon");
    } else {
      const embedded = run(process.execPath, [script, "--bin", binary, "--backend", "embedded"]);
      const live = run(process.execPath, [script, "--bin", binary, "--backend", UPSTREAM]);
      const ok = /VERDICT: PASS/.test(embedded.output) && /VERDICT: PASS/.test(live.output);
      record(
        "harness",
        "snapshot and restore",
        ok ? PASS : FAIL,
        ok
          ? "round trip on the embedded backend, clear refusal on the real one"
          : lastLines(ok ? "" : `${embedded.output}\n${live.output}`, 8),
      );
    }
  }

  // Usage accounting (FR-15) as a round trip: report through `context.record_usage`, read the
  // receipt, check the totals are the sum they claim to be. The classifier has unit tests; the
  // tool path a user takes had none, and a classifier can be right while the tool feeding it
  // is wrong.
  if (wanted("usage", "harness")) {
    const script = join(ROOT, "docs", "verification", "usage-roundtrip.mjs");
    const binary = daemonBinary();
    if (!existsSync(script) || !binary) {
      record("harness", "usage accounting round trip", SKIP, "needs a built daemon");
    } else {
      const r = run(process.execPath, [script, "--bin", binary]);
      const ok = r.ok && /VERDICT: PASS/.test(r.output);
      record(
        "harness",
        "usage accounting round trip",
        ok ? PASS : FAIL,
        ok
          ? "cold, growing, resent, and broken-prefix turns"
          : lastLines(r.output, 8),
      );
    }
  }

  if (wanted("hermes-cli", "harness")) {
    if (!available("hermes", ["--version"])) {
      record("harness", "Hermes plugin", SKIP, "hermes not on PATH");
    } else {
      const r = run("hermes", ["plugins", "list"]);
      // `r.ok` is deliberately not required: a CLI can list plugins and still exit non-zero, and
      // the evidence that matters is whether the plugin appears in the output. Requiring a clean
      // exit would turn a successful listing into "not installed".
      const listed = /sakur4/.test(r.output);
      record(
        "harness",
        "Hermes plugin",
        listed ? PASS : SKIP,
        listed ? "discovered as a user plugin" : "not installed — see integrations/hermes-plugin",
      );
    }
  }
}

// ===========================================================================
// Report
// ===========================================================================

function summarize() {
  const groups = [...new Set(results.map((r) => r.group))];
  const counts = { [PASS]: 0, [FAIL]: 0, [SKIP]: 0 };
  for (const r of results) counts[r.status] += 1;

  process.stdout.write("\n" + "─".repeat(72) + "\n");
  for (const group of groups) {
    const inGroup = results.filter((r) => r.group === group);
    const p = inGroup.filter((r) => r.status === PASS).length;
    const f = inGroup.filter((r) => r.status === FAIL).length;
    const s = inGroup.filter((r) => r.status === SKIP).length;
    const verdict = f > 0 ? "\x1b[31mFAIL\x1b[0m" : s > 0 ? "\x1b[33mINCOMPLETE\x1b[0m" : "\x1b[32mOK\x1b[0m";
    process.stdout.write(
      `  ${group.padEnd(12)} ${String(p).padStart(2)} passed` +
        (f ? `  \x1b[31m${f} failed\x1b[0m` : "") +
        (s ? `  \x1b[33m${s} skipped\x1b[0m` : "") +
        `   ${verdict}\n`,
    );
  }
  process.stdout.write("─".repeat(72) + "\n");
  process.stdout.write(
    `  ${counts[PASS]} passed · ${counts[FAIL]} failed · ${counts[SKIP]} skipped\n`,
  );

  // The distinction this whole script exists to make. A run that skipped checks is not a
  // green run, and saying "0 failed" without saying "3 skipped" is how a gap becomes
  // invisible.
  if (counts[SKIP] > 0) {
    process.stdout.write("\n  Skipped, and why:\n");
    for (const r of results.filter((x) => x.status === SKIP)) {
      process.stdout.write(`    · ${r.id}: ${r.note}\n`);
    }
  }

  if (JSON_OUT) {
    writeFileSync(JSON_OUT, JSON.stringify({ results, counts }, null, 2));
    process.stdout.write(`\n  wrote ${JSON_OUT}\n`);
  }

  return counts;
}

async function main() {
  if (has("list")) {
    process.stdout.write(
      [
        "groups and what they need:",
        "  rust        nothing                      fmt, clippy, tests, doctests, doc",
        "  encryption  OpenSSL development files    FR-20 acceptance criterion",
        "  hermes      python + a built daemon      FR-16 context engine, 44 contracts",
        "  bench       a repository to index        scripted A/B, NFR-2 recall",
        "  live        --upstream <url>             real llama.cpp measurements",
        "  harness     a built daemon               OMP + Hermes install, generated config",
        "                                           paths, the extension's command forms,",
        "                                           and every tool name it references",
        "",
        "--only takes a group or a check id, where a check id is the name printed beside PASS.",
        "`hermes` is a group (the context engine); the plugin-discovery check is `Hermes plugin`",
        "in the harness group. A filter that matches nothing is reported and exits non-zero,",
        "because an empty result set is not a green one.",
        "",
      ].join("\n"),
    );
    return 0;
  }

  process.stdout.write(`Sakur4 verification\n`);
  process.stdout.write(`  root       ${ROOT}\n`);
  process.stdout.write(`  daemon     ${daemonBinary() ?? "not built"}\n`);
  process.stdout.write(`  upstream   ${UPSTREAM ?? "not configured (live checks will skip)"}\n`);
  if (ONLY.length) process.stdout.write(`  only       ${ONLY.join(", ")}\n`);
  process.stdout.write("\n");

  rustChecks();
  encryptionChecks();
  await hermesChecks();
  benchChecks();
  await liveChecks();
  harnessChecks();

  const counts = summarize();

  // # A filter that matched nothing is not a pass
  //
  // `--only nfr --quick` ran no checks at all and printed "0 passed · 0 failed · 0 skipped",
  // which reads as success. It is the third time in this project that an empty result set has
  // presented as a green one — after an empty `--only` in an earlier version and a tool-name
  // check that matched zero call sites. The set of things that ran is now compared against what
  // was asked for.
  //
  // `--quick` legitimately suppresses some checks, so an id that was filtered out rather than
  // unknown is named as such instead of being called a typo.
  if (ONLY.length > 0) {
    const unmatched = ONLY.filter((name) => !recordedGroups.has(name) && !recordedIds.has(name));
    if (unmatched.length > 0) {
      const known = KNOWN_NAMES;
      process.stdout.write(
        `\n  \x1b[33m--only named ${unmatched.map((u) => `'${u}'`).join(", ")}, which matched no check.\x1b[0m\n` +
          `  Known here: ${known.join(", ")}\n` +
          (QUICK ? "  (--quick suppresses some checks; drop it to include them.)\n" : ""),
      );
      return 1;
    }
  }

  // The catalog above is only useful if it matches reality, so it is checked after a full run —
  // where everything that can run has run. A narrow `--only` is exempt, because it excludes by
  // design, and `--quick` is exempt because it suppresses the slow checks.
  if (ONLY.length === 0 && !QUICK) {
    const ran = new Set([...recordedGroups, ...recordedIds]);
    const unlisted = [...ran].filter((name) => !KNOWN_NAMES.includes(name)).sort();
    const unused = KNOWN_NAMES.filter((name) => !ran.has(name));
    if (unlisted.length > 0 || unused.length > 0) {
      process.stdout.write(
        "\n  \x1b[33mthe check catalog in this file is stale.\x1b[0m" +
          (unlisted.length ? ` Not listed: ${unlisted.join(", ")}.` : "") +
          (unused.length ? ` Listed but absent: ${unused.join(", ")}.` : "") +
          "\n",
      );
      return 1;
    }
  }

  if (counts[FAIL] > 0) return 1;
  if (REQUIRE_ALL && counts[SKIP] > 0) {
    process.stdout.write(
      "\n  --require-all was given, and something was skipped. A skipped check is not a\n" +
        "  passed check; see the reasons above.\n",
    );
    return 1;
  }
  return 0;
}

process.exit(await main());
