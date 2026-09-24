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
// # Only the *other* languages, not Rust again
//
// This used to begin with every Rust source, so the per-file pass below — which is careful about
// declarations, comments and fields — was undone by a blunt second pass over the same text. The doc
// comment in `tools.rs` that *mentions* `open_folds` in backticks matched a bare `open_folds(`, and
// the anchor case was filtered out of its own report. A filter that re-reads the corpus with a weaker
// rule than the pass that produced the candidates is worse than no filter.
const elsewhere =
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

    // # Two calling conventions, and the first version only handled one
    //
    // A method is called `x.name(` or `Type::name(`. A **free function** is imported with `use` and
    // then called bare: `snap_to_checkpoint(requested_cut, …)`. Requiring `.` or `::` therefore
    // reported `snap_to_checkpoint` as having no caller while `cache/mod.rs` called it — a false
    // positive, which is the failure mode that gets a list ignored.
    //
    // The earlier version made the opposite mistake: it accepted a bare `name(`, so the struct field
    // `pub open_folds: i64` counted as a use and `open_folds` — genuinely uncalled — was missed.
    //
    // Handling both means accepting a bare call again, and excluding the two things that are not
    // calls but do look like one: a **definition** (`fn name(`) and a **field** (`name:`). Comments
    // are a residual source of false negatives, and that direction is deliberate — a list with a few
    // extra entries gets read, while one that hides a real case does not get read at all.
    const callRe = new RegExp(
      `(\\.|::)${name}\\s*\\(|(^|[^A-Za-z0-9_.:])${name}\\s*\\(`,
      "g",
    );
    // The declaration pattern has to be line-anchored at **both** ends. Without the trailing `$` it
    // matched `pub open_folds:` inside a doc comment that merely *mentions* the field, so a comment
    // cancelled a real call and the anchor case disappeared from the list entirely — the check
    // silently becoming blind to the very thing it was built for.
    const notACall = new RegExp(
      `^\\s*(pub(\\([^)]*\\))?\\s+)?(async\\s+)?fn\\s+${name}\\s*[(<]|^\\s*(pub\\s+)?${name}\\s*:.*$`,
      "m",
    );
    let inSource = 0;
    let inTests = 0;
    for (const other of rustSources) {
      const body = sourceText.get(other);
      // # Per line, because a declaration matches the call pattern
      //
      // `fn open_folds(` *is* `open_folds(`. Counting declarations separately and subtracting them
      // made this miss its own anchor: `evict.rs` matched once as a call and once as a declaration,
      // the subtraction produced zero, and `open_folds` — the function with no caller this was built
      // to find — read as called. Deciding per line removes the arithmetic and the class of mistake
      // with it. Two earlier versions of this predicate were wrong in opposite directions; that is
      // why the known case is asserted rather than assumed.
      let count = 0;
      for (const line of body.split(/\r?\n/)) {
        if (!callRe.test(line)) continue;
        if (notACall.test(line)) continue;
        count += 1;
      }
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
  const re = new RegExp(`(\\.|::)${c.name}\\s*\\(|(^|[^A-Za-z0-9_.:])${c.name}\\s*\\(`, "m");
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

// # It does check itself, though
//
// Four versions of the predicate above were wrong, and three of them were wrong in ways that produced
// a *plausible-looking list* — one reported nothing at all, one hid the very case it was built for,
// one counted a doc comment as a call. None was caught by reading it; each was caught by checking the
// output against something whose answer was already known.
//
// So the known answers are asserted here rather than trusted to whoever runs it next. `open_folds` is
// the canonical positive: it exists, it has a doc comment saying what it is for, and no code calls it.
// `snap_to_checkpoint` is the canonical negative in the harder direction — it *is* called, as a free
// function, which is the convention that a method-only pattern misses. `render_anchor_block` is the
// negative for a defect that has since been fixed, so it must not reappear.
const KNOWN_UNCALLED = ["open_folds", "assess"];
const KNOWN_CALLED = ["snap_to_checkpoint", "render_anchor_block"];

const names = new Set(real.map((c) => c.name));
const missing = KNOWN_UNCALLED.filter((n) => !names.has(n));
const wrong = KNOWN_CALLED.filter((n) => names.has(n));

console.log(`  ${rustSources.length} Rust files scanned`);
console.log(`  ${real.length} candidate(s) — public functions with no caller here.`);
for (const c of real) {
  console.log(`    ${relative(ROOT, c.path)}:${c.line}  ${c.name}`);
}
console.log("  Read the doc comments; a stated purpose with no caller is the class worth finding.");

if (missing.length || wrong.length) {
  console.log("");
  console.log("  THE PREDICATE IS WRONG, so the list above cannot be trusted:");
  for (const n of missing) console.log(`    ${n} is uncalled but was not listed`);
  for (const n of wrong) console.log(`    ${n} has a caller but was listed`);
  process.exit(1);
}
