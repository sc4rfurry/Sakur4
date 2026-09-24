#!/usr/bin/env node
/**
 * Check that every environment variable the documentation names actually exists in the code.
 *
 * # Why this is worth a check
 *
 * An environment variable is a promise: the docs say "set `SAKUR4_EVICTION_PROFILE` to choose a
 * profile" and a reader exports it and expects an effect. A name that is documented and not read
 * fails silently in the worst way — the program behaves as if it were never set, and the reader
 * concludes the feature is broken rather than the documentation.
 *
 * The reverse direction matters less and is not checked: a variable read by the code and not
 * documented is discoverable by reading `.env` handling, and demanding docs for every internal
 * flag would make this noisy.
 *
 * # What counts as a documented variable
 *
 * Upper-case names with at least one underscore, matching a known prefix. The prefix list exists
 * because the docs are also full of identifiers that look like environment variables —
 * `sakur4_commit`, `sakur4_symbol`, `hermes_cli`, `sakur4_core` — and those are tool names,
 * function names and crate names. Without the filter this check reports a dozen false positives
 * and gets ignored, which is how a check dies.
 *
 * Usage:
 *   node docs/verification/env-vars.mjs [--root .]
 */

import { readFileSync, readdirSync, existsSync } from "node:fs";
import { join, relative, extname } from "node:path";

const argv = process.argv.slice(2);
const arg = (name, fallback) => {
  const i = argv.indexOf(`--${name}`);
  return i >= 0 && argv[i + 1] && !argv[i + 1].startsWith("--") ? argv[i + 1] : fallback;
};

const ROOT = arg("root", process.cwd());

// Prefixes that only ever mean an environment variable in this project.
const PREFIXES = ["SAKUR4_", "HERMES_", "LLAMA_", "OMP_"];

const SKIP_DIRS = new Set(["target", "node_modules", ".git", "dist"]);
const DOC_EXTS = new Set([".md"]);
const CODE_EXTS = new Set([".rs", ".mjs", ".js", ".ts", ".sh", ".py", ".ps1", ".yml", ".yaml", ".toml"]);

function* walk(dir, exts) {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    if (SKIP_DIRS.has(entry.name)) continue;
    const full = join(dir, entry.name);
    if (entry.isDirectory()) yield* walk(full, exts);
    else if (entry.isFile() && exts.has(extname(entry.name))) yield full;
  }
}

/** Every upper-case prefixed name that appears in a doc, with where it appeared. */
const documented = new Map();
for (const file of walk(ROOT, DOC_EXTS)) {
  const rel = relative(ROOT, file);
  // The check's own source describes the prefixes it looks for; excluding the verification
  // directory avoids the check reporting itself.
  if (rel.startsWith("docs/verification") || rel.startsWith("docs\\verification")) continue;
  const text = readFileSync(file, "utf8");
  for (const [index, line] of text.split("\n").entries()) {
    for (const match of line.matchAll(/\b([A-Z][A-Z0-9]*(?:_[A-Z0-9]+)+)\b/g)) {
      const name = match[1];
      if (!PREFIXES.some((p) => name.startsWith(p))) continue;
      if (!documented.has(name)) documented.set(name, `${rel}:${index + 1}`);
    }
  }
}

/** Every prefixed name that appears anywhere in code. */
const inCode = new Set();
for (const file of walk(ROOT, CODE_EXTS)) {
  const text = readFileSync(file, "utf8");
  for (const match of text.matchAll(/\b([A-Z][A-Z0-9]*(?:_[A-Z0-9]+)+)\b/g)) {
    if (PREFIXES.some((p) => match[1].startsWith(p))) inCode.add(match[1]);
  }
}

// Names the CLI or a shell script consumes without the word appearing in a `.rs` file — the
// harness configs pass some straight through, so their only "reader" is a documented contract.
const KNOWN_EXTERNAL = new Set([
  // Read by the Hermes plugin and by `verify.mjs`, which are not the daemon.
  "SAKUR4_URL",
  "SAKUR4_BIN",
  "SAKUR4_BIN_DIR",
  "HERMES_AGENT_DIR",
  "HERMES_HOME",
  "HERMES_OK",
]);

let missing = 0;
const rows = [];
for (const [name, where] of [...documented].sort()) {
  if (inCode.has(name) || KNOWN_EXTERNAL.has(name)) continue;
  missing += 1;
  rows.push(`  ${name}  documented at ${where}`);
}

console.log(`  ${documented.size} documented environment variable(s), ${inCode.size} name(s) in code`);
console.log("");
if (missing === 0) {
  console.log("every documented environment variable is read somewhere");
  process.exit(0);
}
console.log(`${missing} documented variable(s) appear nowhere in the code — setting them does nothing:`);
for (const row of rows) console.log(row);
process.exit(1);
