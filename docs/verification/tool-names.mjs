#!/usr/bin/env node
/**
 * Check that every tool name the harness integrations use exists in the daemon's catalog.
 *
 * The OMP extension and the Hermes engine both call MCP tools by name. A typo or a rename is
 * invisible until someone runs that specific tool inside a session — the rest of the integration
 * keeps working, so the plugin looks installed and healthy while one tool is dead.
 *
 * This reads the names from the integration sources rather than from a list kept here, so it
 * cannot drift: a new call site is covered the moment it is written.
 */

import { spawnSync } from "node:child_process";
import { readFileSync, existsSync } from "node:fs";
import { join, resolve } from "node:path";

const root = resolve(import.meta.dirname, "..", "..");
const bin =
  process.argv[2] ?? `${process.env.USERPROFILE ?? process.env.HOME}/.cargo/bin/sakur4d`;

/** The daemon's advertised tools, straight from `tools/list`. */
function catalog() {
  const frames = [
    { jsonrpc: "2.0", id: 1, method: "initialize", params: { protocolVersion: "2025-11-25", capabilities: {}, clientInfo: { name: "check", version: "1" } } },
    { jsonrpc: "2.0", method: "notifications/initialized", params: {} },
    { jsonrpc: "2.0", id: 2, method: "tools/list", params: {} },
  ];
  const result = spawnSync(bin, ["--db", ":memory:", "--backend", "none", "serve", "--transport", "stdio"], {
    encoding: "utf8",
    input: frames.map((f) => JSON.stringify(f)).join("\n") + "\n",
    timeout: 120_000,
    maxBuffer: 64 * 1024 * 1024,
  });
  for (const line of (result.stdout ?? "").split("\n")) {
    if (!line.trim().startsWith("{")) continue;
    let parsed;
    try {
      parsed = JSON.parse(line);
    } catch {
      continue;
    }
    if (parsed.id === 2) return new Set((parsed.result?.tools ?? []).map((t) => t.name));
  }
  return new Set();
}

/**
 * Tool names referenced in a source file, in any of the shapes the integrations use.
 *
 * `client.tool("…")` for the TypeScript extension, and `self.client.call("…")` or
 * `client.call("…")` for the Python engine. A pattern per language rather than one clever
 * one, because a check that silently finds nothing reports success — the Python side was
 * invisible until this was widened, and "0 unknown" from 0 matches is not a pass.
 */
function usedBy(path) {
  if (!existsSync(path)) return [];
  const source = readFileSync(path, "utf8");
  const names = new Set();
  for (const match of source.matchAll(/client\.tool\(\s*"([^"]+)"/g)) names.add(match[1]);
  for (const match of source.matchAll(/client\.call\(\s*"([^"]+)"/g)) names.add(match[1]);
  return [...names].sort();
}

const known = catalog();
if (known.size === 0) {
  console.error("could not read the tool catalog from the daemon");
  process.exit(2);
}
console.log(`daemon advertises ${known.size} tools`);

const integrations = [
  ["OMP extension", join(root, "integrations", "omp-plugin", "index.ts")],
  ["Hermes engine", join(root, "integrations", "hermes-plugin", "__init__.py")],
];

let missing = 0;
for (const [label, path] of integrations) {
  const used = usedBy(path);
  if (used.length === 0) {
    console.log(`  ${label.padEnd(16)} (no MCP tool calls found)`);
    continue;
  }
  const bad = used.filter((name) => !known.has(name));
  missing += bad.length;
  console.log(
    `  ${label.padEnd(16)} ${used.length} tool name(s) used, ${bad.length} unknown` +
      (bad.length ? `: ${bad.join(", ")}` : ""),
  );
}

console.log("");
if (missing === 0) {
  console.log("every tool name used by an integration exists in the catalog");
  process.exit(0);
}
console.log(`${missing} tool name(s) do not exist — those tools fail at runtime only`);
process.exit(1);
