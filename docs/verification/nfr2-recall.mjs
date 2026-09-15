// NFR-2: `memory.recall` under 300 ms with 100,000 episodic entries.
//
// # Why this is a script and not a unit test
//
// The requirement is a latency bound at a scale no test fixture should carry: a
// hundred thousand episodes is a 100 MB-ish store and tens of seconds of writing.
// Putting that in the suite would mean every `cargo test` run pays for it. So it runs
// here, on demand, against a real store — and reports the distribution rather than a
// single number, because a mean hides the tail and the tail is what a user feels.
//
// # What it does
//
//  1. Writes N episodes through the running daemon, in batches, timing the writes.
//  2. Runs a spread of queries — common words, rare words, exact phrases, nonsense —
//     and reports min / median / p95 / max latency for each class.
//  3. Compares against the 300 ms target.
//
// Query classes matter because hybrid retrieval is not uniform: a term that appears
// in every episode exercises the ranker, while a term in one episode exercises the
// index. Reporting only the average would hide whichever of those is slow.
//
// Run:
//   node docs/verification/nfr2-recall.mjs --db /path/to/store.db [--n 100000]
//
// It spawns its own daemon on a free port and cleans up after itself.

import { spawn } from "node:child_process";
import { existsSync, rmSync, statSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

function arg(name, fallback) {
  const i = process.argv.indexOf(name);
  return i >= 0 && process.argv[i + 1] ? process.argv[i + 1] : fallback;
}

const N = Number(arg("--n", "100000"));
const K = Number(arg("--k", "8"));
const PORT = Number(arg("--port", "8931"));
const DB = arg("--db", join(process.env.TEMP ?? "/tmp", `sakur4-nfr2-${Date.now()}.db`));
const BIN =
  arg("--bin", null) ??
  [
    join(homedir(), ".cargo", "bin", process.platform === "win32" ? "sakur4d.exe" : "sakur4d"),
    join(process.cwd(), "target", "release", process.platform === "win32" ? "sakur4d.exe" : "sakur4d"),
  ].find(existsSync);

if (!BIN) {
  console.error("could not find sakur4d; pass --bin");
  process.exit(2);
}

const BASE = `http://127.0.0.1:${PORT}`;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

console.log(`binary   ${BIN}`);
console.log(`store    ${DB}`);
console.log(`entries  ${N.toLocaleString()}`);
console.log("");

const child = spawn(
  BIN,
  ["--db", DB, "--backend", "embedded", "serve", "--transport", "http", "--bind", `127.0.0.1:${PORT}`, "--no-dream"],
  { stdio: ["ignore", "ignore", "pipe"] },
);
let daemonErr = "";
child.stderr.on("data", (d) => (daemonErr += d.toString()));

const cleanup = () => {
  try {
    child.kill();
  } catch {
    /* already gone */
  }
};
process.on("exit", cleanup);
process.on("SIGINT", () => {
  cleanup();
  process.exit(130);
});

// Wait for the port to answer.
let ready = false;
for (let i = 0; i < 60; i++) {
  await sleep(250);
  try {
    await call("sakur4.status", {});
    ready = true;
    break;
  } catch {
    /* not up yet */
  }
}
if (!ready) {
  console.error(`daemon did not come up.\n${daemonErr.slice(0, 800)}`);
  cleanup();
  process.exit(1);
}

/** One MCP tool call over the daemon's own HTTP transport. */
async function call(name, args) {
  const res = await fetch(`${BASE}/`, {
    method: "POST",
    headers: {
      "content-type": "application/json",
      accept: "application/json, text/event-stream",
      "MCP-Protocol-Version": "2026-07-28",
      "Mcp-Method": "tools/call",
      "Mcp-Name": name,
    },
    body: JSON.stringify({
      jsonrpc: "2.0",
      id: 1,
      method: "tools/call",
      params: {
        name,
        arguments: args,
        _meta: {
          "io.modelcontextprotocol/protocolVersion": "2026-07-28",
          "io.modelcontextprotocol/clientCapabilities": {},
        },
      },
    }),
  });
  const text = await res.text();
  // Streamable HTTP may answer as SSE; take the last data frame.
  const payload = text
    .split("\n")
    .filter((l) => l.startsWith("data: "))
    .map((l) => l.slice(6))
    .pop();
  const parsed = JSON.parse(payload ?? text);
  if (parsed.error) throw new Error(`${name}: ${parsed.error.message}`);
  return parsed.result?.structuredContent ?? JSON.parse(parsed.result?.content?.[0]?.text ?? "{}");
}

// ---------------------------------------------------------------------------
// Vocabulary. Split so some terms are ubiquitous and some are unique, which is
// what makes the retrieval classes differ.
// ---------------------------------------------------------------------------
const COMMON = ["refactor", "handler", "validate", "payload", "timeout", "retry", "cache", "index"];
const TOPICS = ["auth", "billing", "search", "ingest", "export", "notify", "audit", "sync"];

function episode(i) {
  const common = COMMON[i % COMMON.length];
  const topic = TOPICS[i % TOPICS.length];
  return (
    `turn ${i}: ${common} the ${topic} path — touched src/${topic}/mod_${i % 977}.rs, ` +
    `changed signature of process_${i} and re-ran the ${topic} suite; ` +
    `note token_${i} and marker_${i} for later.`
  );
}

console.log("seeding…");
const started = Date.now();
const BATCH = 500;
for (let i = 0; i < N; i += BATCH) {
  // Sequential, not parallel: this measures the store's write path, and issuing
  // concurrent writes would measure contention in this script instead.
  for (let j = i; j < Math.min(i + BATCH, N); j++) {
    await call("memory.commit_episode", {
      role: j % 3 === 0 ? "tool" : "user",
      tool_name: j % 3 === 0 ? "read_file" : undefined,
      content: episode(j),
      session_id: "nfr2",
    });
  }
  if ((i / BATCH) % 20 === 0) {
    process.stdout.write(
      `  ${String(Math.min(i + BATCH, N)).padStart(7)} / ${N.toLocaleString()}` +
        `  (${((Date.now() - started) / 1000).toFixed(0)}s)\r`,
    );
  }
}
const seedSecs = (Date.now() - started) / 1000;
process.stdout.write(" ".repeat(60) + "\r");
console.log(`seeded ${N.toLocaleString()} episodes in ${seedSecs.toFixed(0)}s`);

const status = await call("sakur4.status", {});
console.log(`store now holds ${status.episodes.toLocaleString()} episodes`);
if (existsSync(DB)) {
  console.log(`store size  ${(statSync(DB).size / 1024 / 1024).toFixed(0)} MB`);
}
console.log("");

// ---------------------------------------------------------------------------
// Measurement
// ---------------------------------------------------------------------------
const queries = [
  ["ubiquitous term", () => COMMON[Math.floor(Math.random() * COMMON.length)]],
  ["mid-frequency term", () => TOPICS[Math.floor(Math.random() * TOPICS.length)]],
  ["unique token", () => `token_${Math.floor(Math.random() * N)}`],
  ["unique marker", () => `marker_${Math.floor(Math.random() * N)}`],
  ["two-term phrase", () => `${COMMON[Math.floor(Math.random() * COMMON.length)]} ${TOPICS[Math.floor(Math.random() * TOPICS.length)]}`],
  ["absent term", () => `zzz_nonexistent_${Math.floor(Math.random() * 1e6)}`],
];

const REPS = Number(arg("--reps", "12"));
const stats = (xs) => {
  const s = [...xs].sort((a, b) => a - b);
  const q = (p) => s[Math.min(s.length - 1, Math.floor(s.length * p))];
  return { min: s[0], median: q(0.5), p95: q(0.95), max: s[s.length - 1] };
};

console.log(`recall latency, k=${K}, ${REPS} reps per class`);
console.log("");
console.log("  query class            min     median      p95      max    verdict");

let worst = 0;
for (const [label, make] of queries) {
  const times = [];
  for (let r = 0; r < REPS; r++) {
    const q = make();
    const t0 = performance.now();
    await call("memory.recall", { query: q, k: K, session_id: "nfr2" });
    times.push(performance.now() - t0);
  }
  const s = stats(times);
  worst = Math.max(worst, s.p95);
  console.log(
    `  ${label.padEnd(20)} ${s.min.toFixed(0).padStart(6)}ms ${s.median.toFixed(0).padStart(9)}ms ` +
      `${s.p95.toFixed(0).padStart(8)}ms ${s.max.toFixed(0).padStart(8)}ms    ` +
      (s.p95 < 300 ? "pass" : "FAIL"),
  );
}

console.log("");
console.log(`worst p95 across classes: ${worst.toFixed(0)} ms   (NFR-2 target: < 300 ms)`);
console.log(
  worst < 300
    ? `VERDICT: PASS at ${N.toLocaleString()} entries`
    : `VERDICT: FAIL at ${N.toLocaleString()} entries`,
);

cleanup();
rmSync(DB, { force: true });
rmSync(`${DB}-wal`, { force: true });
rmSync(`${DB}-shm`, { force: true });
