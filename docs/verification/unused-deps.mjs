#!/usr/bin/env node
// Every dependency a crate declares is used somewhere in it.
//
// # Why this exists
//
// `sakur4d` declared `tower` and `tower-http` — with the `trace` and `cors` features — and referenced
// neither anywhere in its source. The HTTP surface uses `axum::http` and `axum`'s own routing, so
// both sat in the manifest, compiled, and shipped as dead weight in every release archive.
//
// It was found by updating `tower-http` to a new major version and noticing that nothing in the tree
// mentioned it. That is a bad way to find it: the update was Dependabot's, the failure mode of
// getting it wrong is a build break a long way from the cause, and the same audit had never been run.
// A dependency that is declared and unused is not harmless either — it pins a version range the
// project does not use, contributes its features to the build, and appears in the lockfile as if it
// mattered.
//
// # What counts as used
//
// A mention of the crate's underscore name as a path segment: `tower::`, `tower_http::`, or in a
// `use` line. Deliberately simple, because the alternative is resolving Rust, and a miss here
// produces a false report that a person then checks — which is the right way round.
//
// Dev-dependencies are checked against the crate's tests and benches, not its source, so
// `sakur4-testkit` does not look unused for being used only from `tests/`.
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join, basename, dirname, relative } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..", "..");
const cratesDir = join(ROOT, "crates");

/** Every .rs file under a directory. */
function rustFiles(dir, out = []) {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) {
      if (entry.name === "target" || entry.name === "node_modules") continue;
      rustFiles(path, out);
    } else if (entry.name.endsWith(".rs")) {
      out.push(path);
    }
  }
  return out;
}

/** Dependency names a Cargo.toml declares, by table. */
function declared(toml) {
  const out = { dependencies: [], dev: [] };
  let table = null;
  for (const raw of toml.split(/\r?\n/)) {
    const line = raw.trim();
    if (line.startsWith("[")) {
      table = line.match(/^\[(dev-)?dependencies\]$/) ? (line.includes("dev-") ? "dev" : "dependencies") : null;
      continue;
    }
    if (!table || !line || line.startsWith("#")) continue;
    // # The key may contain a dot
    //
    // `tokio.workspace = true` is how a workspace-inherited dependency is written, and the first
    // version of this matched `^([A-Za-z0-9_-]+)\s*=` — no dot allowed — so it skipped every
    // inherited dependency. That is **most** of them here: the check reported "all 3 declared
    // dependencies, across 3 crate(s)" while `sakur4-core` alone declares thirty. An audit that reads
    // a fraction of the manifest and prints a clean result is worse than no audit.
    const m = line.match(/^([A-Za-z0-9_-]+)(\.[A-Za-z0-9_-]+)?\s*=/);
    if (m) out[table].push(m[1]);
  }
  return out;
}

/** Crates that are not Rust libraries, or whose name does not map to a module path. */
const NOT_A_LIBRARY = new Set(["sakur4-core", "sakur4d", "sakur4-testkit"]);

const problems = [];
let crateCount = 0;
let checked = 0;

for (const crate of readdirSync(cratesDir)) {
  const crateDir = join(cratesDir, crate);
  const manifest = join(crateDir, "Cargo.toml");
  try {
    if (!statSync(crateDir).isDirectory() || !statSync(manifest).isFile()) continue;
  } catch {
    continue;
  }

  crateCount += 1;
  const { dependencies, dev } = declared(readFileSync(manifest, "utf8"));
  const all = rustFiles(crateDir);
  const source = all.filter((p) => !p.includes(`${join("", "tests")}`) && !p.includes(`${join("", "benches")}`));
  const testOnly = all.filter((p) => p.includes("tests") || p.includes("benches"));

  const text = (files) => files.map((p) => readFileSync(p, "utf8")).join("\n");
  const sourceText = text(source.length ? source : all);
  const testText = text(testOnly.length ? testOnly : all) + "\n" + sourceText;

  const used = (name, haystack) => {
    const module = name.replace(/-/g, "_");
    // # A `use` line counts, and it is the common case
    //
    // The first version looked only for `module::`, which misses `use serde::{Deserialize, Serialize}`
    // — the name is followed by `::{` not `::`. That produced *false positives*: `serde` (92 uses),
    // `tracing` (33) and `toml` (3) were all reported unused, which is the failure mode that gets a
    // check switched off. Caught before anything was removed, by grepping the names the report
    // claimed were absent.
    //
    // The `:` exclusion still matters: without it `axum::http::` reads as a use of `http`.
    return (
      new RegExp(`(^|[^A-Za-z0-9_:])${module}::`, "m").test(haystack) ||
      new RegExp(`^\\s*use\\s+${module}\\b`, "m").test(haystack) ||
      new RegExp(`^\\s*(pub\\s+)?use\\s+${module}\\b`, "m").test(haystack)
    );
  };

  for (const name of dependencies) {
    checked += 1;
    if (!used(name, sourceText)) {
      problems.push(`${relative(ROOT, manifest)}: \`${name}\` is declared but never referenced in ${relative(ROOT, crateDir)}/src`);
    }
  }
  for (const name of dev) {
    checked += 1;
    if (!used(name, testText)) {
      problems.push(`${relative(ROOT, manifest)}: dev-dependency \`${name}\` is never referenced in tests or source`);
    }
  }
}

if (problems.length) {
  console.log(`  ${checked} declared across ${crateCount} crate(s), ${problems.length} unused`);
  for (const p of problems) console.log(`    ${p}`);
  process.exit(1);
}
console.log(`  all ${checked} declared dependencies, across ${crateCount} crate(s), are referenced`);
