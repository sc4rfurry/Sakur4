#!/usr/bin/env node
/**
 * Check that the README's tool table matches the argument names the tools actually declare.
 *
 * # Why this exists
 *
 * The README documents 17 tools with their required and optional arguments. It is the reference a
 * reader uses to write a call, so a missing argument is not cosmetic: `code.get_repo_map` accepts
 * `names_only`, and a reader who cannot discover that has no way to learn the qualified names that
 * `code.query_symbol` and `code.impact_of_change` require. The tool is unreachable in practice
 * from the documentation that describes it.
 *
 * Comparing the table against the Rust input structs is mechanical, and this is the second
 * argument-name drift found by hand. The first was `--names` on the skill CLI, documented and
 * never implemented.
 *
 * # What it checks
 *
 * For each row, every argument named in the row appears as a field of that tool's input struct.
 * It does not check the reverse direction — a tool may legitimately accept fields the README
 * omits from the summary — so this catches a *wrong* name, which is the failure that wastes a
 * reader's time, rather than an incomplete one.
 *
 * Usage:
 *   node docs/verification/tool-args.mjs [--root .]
 */

import { readFileSync, existsSync } from "node:fs";
import { join } from "node:path";

const argv = process.argv.slice(2);
const arg = (name, fallback) => {
  const i = argv.indexOf(`--${name}`);
  return i >= 0 && argv[i + 1] && !argv[i + 1].startsWith("--") ? argv[i + 1] : fallback;
};

const ROOT = arg("root", process.cwd());
const README = join(ROOT, "README.md");
const TOOLS = join(ROOT, "crates", "sakur4d", "src", "tools.rs");

for (const path of [README, TOOLS]) {
  if (!existsSync(path)) {
    console.error(`missing ${path}`);
    process.exit(2);
  }
}

const readme = readFileSync(README, "utf8");
const tools = readFileSync(TOOLS, "utf8");

// --- the documented table ---------------------------------------------------
// Rows look like: | `code.get_repo_map` | `token_budget` | `focus_paths`, `names_only` |
//
// Only the three tool namespaces count. The README also has tables naming things like `llama.cpp`
// and `sakur4.db` in backticks, and a looser pattern treats those as tools and reports them
// missing — which is how the first version of this check produced two false positives alongside
// its real finding.
const NAMESPACES = ["code.", "context.", "memory.", "session.", "sakur4."];

const documented = new Map();
for (const line of readme.split("\n")) {
  const m = line.match(/^\|\s*`([a-z0-9_.]+)`\s*\|\s*([^|]*)\|\s*([^|]*)\|\s*$/i);
  if (!m) continue;
  const [, name, required, optional] = m;
  if (!NAMESPACES.some((ns) => name.startsWith(ns))) continue;
  const names = (text) =>
    [...text.matchAll(/`([a-z0-9_]+)`/gi)].map((x) => x[1]).filter((n) => n !== "—");
  documented.set(name, [...names(required), ...names(optional)]);
}

if (documented.size === 0) {
  console.error("no tool rows found in README.md");
  process.exit(2);
}

// --- the declared structs ---------------------------------------------------
/**
 * Field names of the input struct for a tool.
 *
 * The struct is declared *before* the `#[tool(name = "...")]` attribute that references it, so
 * searching forward from the tool name finds the next tool's struct — which is what the first
 * version of this script did, and why it reported that no tool had a struct at all. The reliable
 * anchor is the handler's own signature, which names its input type directly.
 */
function fieldsFor(tool) {
  const at = tools.indexOf(`name = "${tool}"`);
  if (at < 0) return null;

  // The handler follows the attribute: `async fn name(\n ... Parameters(input): Parameters<X>,`
  const after = tools.slice(at);
  const handler = after.match(/async\s+fn\s+\w+\s*\(([\s\S]{0,400}?)\)\s*->/);
  if (!handler) return null;

  const inputType = handler[1].match(/Parameters\s*<\s*(\w+)\s*>/);
  // A tool may take no arguments at all — `sakur4.status` is `async fn status(&self)`. An empty
  // set is the correct answer for it, and treating that as "no struct found" would make a
  // zero-argument tool indistinguishable from a parse failure.
  if (!inputType) return new Set();

  // Now find that struct's declaration anywhere in the file.
  const declaration = new RegExp(`pub\\s+struct\\s+${inputType[1]}\\s*\\{`);
  const structAt = tools.search(declaration);
  if (structAt < 0) return null;

  const body = tools.slice(structAt);
  const open = body.indexOf("{");
  let depth = 0;
  let end = open;
  for (let i = open; i < body.length; i += 1) {
    if (body[i] === "{") depth += 1;
    else if (body[i] === "}") {
      depth -= 1;
      if (depth === 0) {
        end = i;
        break;
      }
    }
  }
  const text = body.slice(open, end);
  // Strip line comments so a field name mentioned in prose is not counted.
  const code = text
    .split("\n")
    .map((l) => l.replace(/\/\/.*$/, ""))
    .join("\n");
  return new Set([...code.matchAll(/pub\s+([a-z0-9_]+)\s*:/gi)].map((m) => m[1]));
}

// --- compare ----------------------------------------------------------------
let problems = 0;
let checked = 0;

for (const [tool, args] of documented) {
  const fields = fieldsFor(tool);
  if (fields === null) {
    problems += 1;
    console.log(`  ${tool}: no input struct found in tools.rs`);
    continue;
  }
  checked += 1;
  for (const name of args) {
    if (!fields.has(name)) {
      problems += 1;
      console.log(`  ${tool}: README documents '${name}', which is not a field of its input`);
      console.log(`      fields are: ${[...fields].join(", ")}`);
    }
  }
}

console.log("");
if (problems === 0) {
  console.log(`${checked} documented tool(s); every argument name matches a declared field`);
  process.exit(0);
}
console.log(`${problems} argument name(s) in the README do not exist — a reader would pass one and be rejected`);
process.exit(1);
