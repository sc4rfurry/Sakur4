#!/usr/bin/env node
/**
 * Check that every claim about how many tools, resources and prompts the server exposes is true.
 *
 * # Why the count drifts, and why that matters
 *
 * "17 tools, 4 resources, 1 prompt" appears in the README, the crate README, `DESIGN.md` and the
 * CHANGELOG. Each is a claim a reader can check in one command, and each goes stale the moment a
 * tool is added or removed — which is exactly what happened to the *test* count, quoted in four
 * places at three different values.
 *
 * A wrong number here is small and corrosive: it tells the reader the author is not checking, and
 * it makes the numbers that *are* right harder to believe.
 *
 * # How it checks
 *
 * It asks the built daemon, over MCP, what it advertises — the same call a client makes — and then
 * reads each document's claim. That ordering matters: the daemon is the source of truth, not a
 * constant in this file, so adding a tool cannot make this check pass by being updated in step with
 * the thing it is supposed to catch.
 *
 * Usage:
 *   node docs/verification/catalog-counts.mjs [--bin PATH]
 */

import { spawn } from "node:child_process";
import { readFileSync, existsSync } from "node:fs";
import { join } from "node:path";

const argv = process.argv.slice(2);
const arg = (name, fallback) => {
  const i = argv.indexOf(`--${name}`);
  return i >= 0 && argv[i + 1] && !argv[i + 1].startsWith("--") ? argv[i + 1] : fallback;
};

const ROOT = process.cwd();
const EXE = process.platform === "win32" ? "sakur4d.exe" : "sakur4d";
const BIN = arg("bin", [
  join(ROOT, "target", "release", EXE),
  join(ROOT, "target", "debug", EXE),
  join(process.env.USERPROFILE ?? process.env.HOME ?? "", ".cargo", "bin", EXE),
].find(existsSync) ?? "");

if (!BIN || !existsSync(BIN)) {
  console.error("  no sakur4d found; build one with `cargo build --release -p sakur4d`");
  process.exit(2);
}

/** One MCP session, returning the `tools/list`, `resources/list` and `prompts/list` results. */
async function askDaemon() {
  const child = spawn(BIN, ["--db", ":memory:", "--backend", "none", "serve", "--transport", "stdio", "--no-dream"], {
    stdio: ["pipe", "pipe", "ignore"],
  });

  const frames = [
    { jsonrpc: "2.0", id: 1, method: "initialize", params: { protocolVersion: "2025-11-25", capabilities: {}, clientInfo: { name: "counts", version: "1" } } },
    { jsonrpc: "2.0", method: "notifications/initialized", params: {} },
    { jsonrpc: "2.0", id: 2, method: "tools/list", params: {} },
    { jsonrpc: "2.0", id: 3, method: "resources/list", params: {} },
    { jsonrpc: "2.0", id: 4, method: "prompts/list", params: {} },
  ];
  for (const frame of frames) child.stdin.write(JSON.stringify(frame) + "\n");

  const counts = {};
  let buffer = "";
  await new Promise((resolve) => {
    const done = setTimeout(resolve, 20_000);
    child.stdout.on("data", (chunk) => {
      buffer += chunk.toString();
      let index;
      while ((index = buffer.indexOf("\n")) >= 0) {
        const line = buffer.slice(0, index).trim();
        buffer = buffer.slice(index + 1);
        if (!line.startsWith("{")) continue;
        let parsed;
        try {
          parsed = JSON.parse(line);
        } catch {
          continue;
        }
        if (parsed.id === 2) counts.tools = parsed.result?.tools?.length;
        if (parsed.id === 3) counts.resources = parsed.result?.resources?.length;
        if (parsed.id === 4) counts.prompts = parsed.result?.prompts?.length;
        if (counts.tools !== undefined && counts.resources !== undefined && counts.prompts !== undefined) {
          clearTimeout(done);
          resolve();
        }
      }
    });
    child.on("close", () => {
      clearTimeout(done);
      resolve();
    });
  });
  try {
    child.kill();
  } catch {
    /* gone */
  }
  return counts;
}

const counts = await askDaemon();
if (counts.tools === undefined) {
  console.error("  the daemon did not answer tools/list");
  process.exit(1);
}

console.log(`  the daemon advertises ${counts.tools} tools, ${counts.resources} resources, ${counts.prompts} prompt(s)`);
console.log("");

// Each document that states a number, and the pattern that captures it. A file may legitimately
// stop mentioning the count — a removed claim is not a wrong one — so a missing match is reported
// only as information.
const CLAIMS = [
  ["README.md", /(\d+) tools\*\*[^\n]*\*\*(\d+) resources\*\*[^\n]*\*\*(\d+) prompt/],
  ["crates/sakur4d/README.md", /(\d+) MCP tools, (\d+) resources, and (\d+) prompt/],
  ["CHANGELOG.md", /- (\d+) tools, (\d+) resources, and (\d+) prompt/],
  ["docs/DESIGN.md", /`tools\.rs`; (\d+) tools/],
];

let wrong = 0;
for (const [file, pattern] of CLAIMS) {
  const path = join(ROOT, file);
  if (!existsSync(path)) continue;
  const match = readFileSync(path, "utf8").match(pattern);
  if (!match) {
    console.log(`  ${file}: no count stated`);
    continue;
  }
  const stated = Number(match[1]);
  if (stated !== counts.tools) {
    wrong += 1;
    console.log(`  ${file}: says ${stated} tools, the daemon advertises ${counts.tools}`);
    continue;
  }
  // Where a document also states resources and prompts, check those too.
  const rest = match.slice(2).map(Number).filter((n) => !Number.isNaN(n));
  const expected = [counts.resources, counts.prompts].filter((n) => n !== undefined);
  const mismatch = rest.some((n, i) => expected[i] !== undefined && n !== expected[i]);
  if (mismatch) {
    wrong += 1;
    console.log(`  ${file}: says ${rest.join("/")} resources/prompts, actual ${expected.join("/")}`);
  } else {
    console.log(`  ${file}: ${stated} tools — correct`);
  }
}

console.log("");
if (wrong === 0) {
  console.log("every stated catalogue size matches what the daemon advertises");
  process.exit(0);
}
console.log(`${wrong} documented count(s) disagree with the daemon`);
process.exit(1);
