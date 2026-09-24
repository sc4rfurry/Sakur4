#!/usr/bin/env node
// `pub fn`s that nothing in this repository calls.
//
// # Why this is worth a script rather than a reading
//
// Three separate defects in this project were functions that existed, carried a doc comment saying
// what they were for, and had **no caller**:
//
//   * `EvictionEngine::open_folds` — the only code that can list a session's open folds
//   * `RepoCortex::last_indexed` — documented as being "for the repo-map resource TTL", which was a
//     constant, so the resource advertised a relationship that did not exist
//   * `ImpactEntry.stale` — reachable, but a constant `false`, and displayed nowhere
//
// Each was found by reading rather than by removing, and the doc comment was what made them findable:
// a function with no caller and no explanation is clutter, while a function with no caller **and a
// stated purpose** is a promise the code does not keep. This lists the second kind, because reading
// them one at a time does not scale and the compiler will not do it for a published crate.
//
// # What this is not
//
// Not a dead-code verdict. A `pub fn` in `sakur4-core` is part of a library's surface even when this
// workspace does not call it, and tests, benches and the OMP/Hermes plugins are all callers that live
// outside `crates/*/src`. Everything here is a **candidate to read**, and the report says which kind
// of evidence it has: no call site anywhere, or call sites only in tests.
import { readdirSync, readFileSync, statSync } from "node:fs";
import { dirname, join, relative } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..", "..");

function filesUnder(dir, filter, out = []) {
  let entries;
  try {
    entries = readdirSync(dir, { withFileTypes: true });
  } catch {
    return out;
  }
  for (const entry of entries) {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) {
      if (entry.name === "target" || entry.name === "node_modules" || entry.name === ".git") continue;
      filesUnder(path, filter, out);
    } else if (filter(entry.name)) out.push(path);
  }
  return out;
}

const rustSources = filesUnder(join(ROOT, "crates"), (n) => n.endsWith(".rs"));
const sourceText = new Map(rustSources.map((p) => [p, readFileSync(p, "utf8")]));
// Everything that could call something: Rust, the plugins, the skill, and the docs' examples.
const elsewhere =
  rustSources.map((p) => sourceText.get(p)).join("\n") +
  filesUnder(join(ROOT, "integrations"), (n) => n.endsWith(".py") || n.endsWith(".ts"))
    .map((p) => readFileSync(p, "utf8"))
    .join("\n") +
  filesUnder(join(ROOT, "skills"), (n) => n.endsWith(".mjs") || n.endsWith(".md"))
    .map((p) => readFileSync(p, "utf8"))
    .join("\n");

const isTest = (p) => /\/(tests|benches)\//.test(p.replace(/\\/g, "/"));

const candidates = [];
for (const path of rustSources) {
  const text = sourceText.get(path);
  const lines = text.split(/\r?\n/);
  for (let i = 0; i < lines.length; i += 1) {
    const m = lines[i].match(/^\s*pub (?:async )?fn ([a-z_][A-Za-z0-9_]*)\b/);
    if (!m) continue;
    const name = m[1];
    // Trait impls and derive helpers are not free functions; `new`/`default` are constructed by name.
    let inTraitImpl = false;
    for (let back = i; back >= 0 && back > i - 400; back -= 1) {
      if (/^\s*impl\s/.test(lines[back])) {
        inTraitImpl = /^\s*impl\s+[A-Za-z0-9_:<>,\s]+ for /.test(lines[back]);
        break;
      }
    }
    if (inTraitImpl) continue;
    if (["new", "default", "fmt", "from", "drop"].includes(name)) continue;

    // # A call site looks like `.name(` or `::name(`, not `name(`
    //
    // The first version accepted a bare `name(` preceded by any non-word character, so a **struct
    // field** counted as a call: `pub open_folds: i64` in the status output, and a doc comment
    // mentioning the name in backticks, both read as callers. It reported "0 public functions with no
    // caller" while `EvictionEngine::open_folds` — the only code that can list a session's open
    // folds — had none. A detector that counts a field name as a use is the same failure as the
    // dependency audit's, and it is the reason that audit is now checked against a known case before
    // being believed.
    const callRe = new RegExp(`(\\.|::)${name}\\s*\\(`, "g");
    let inSource = 0;
    let inTests = 0;
    for (const other of rustSources) {
      const isOwnFile = other === path;
      const occurrences = sourceText.get(other).match(callRe) ?? [];
      if (!occurrences.length) continue;
      // Subtract the definition itself where it lands in the same file.
      const count = occurrences.length - (isOwnFile ? 1 : 0);
      if (count <= 0) continue;
      if (isTest(other)) inTests += count;
      else inSource += count;
    }
    if (inSource === 0 && inTests === 0) {
      candidates.push({ kind: "no caller anywhere", name, path, line: i + 1 });
    }
  }
}

// Docs and plugins count as callers too, so filter those out of the "no caller" bucket.
const real = candidates.filter((c) => {
  const re = new RegExp(`(\\.|::)${c.name}\\s*\\(`, "m");
  return !re.test(elsewhere);
});

// # This reports candidates, and is deliberately NOT wired into `verify.mjs`
//
// The first working version listed 85 public functions with no caller in this repository. The defects
// this idea came from are among them — `open_folds`, `assess`, `with_env` — but so is most of
// `sakur4-core`'s legitimate library surface: getters (`config`, `base_url`), constructors
// (`with_n_ctx`, `with_weight`), and helpers a published crate is entitled to expose without calling
// itself. A check that fails on all 85 would be switched off within a week, and a check that is
// switched off is worse than one never written — the lesson the dependency audit taught twice in a
// single round.
//
// What makes a candidate readable is the doc comment, and that is the part a script cannot judge. So
// this stays a tool a person runs and reads: take a name, read its comment, and ask whether the
// comment describes something the project claims to do. All three found so far had a stated purpose
// and no code serving it.
console.log(`  ${rustSources.length} Rust files scanned`);
console.log(`  ${real.length} candidate(s) — public functions with no caller here.`);
console.log("  Read the doc comments; a stated purpose with no caller is the class worth finding.");
for (const c of real) {
  console.log(`    ${relative(ROOT, c.path)}:${c.line}  ${c.name}`);
}
