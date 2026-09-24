#!/usr/bin/env node
// The four prompt-building paths still go through the anchor budget check.
//
// # Why a source check rather than a behaviour test
//
// `render_anchor_block` is the only place FR-4's guarantee lives — it returns `BudgetOverflow` when
// the pinned anchors cannot fit, and orders them by kind priority. For the life of the project
// **nothing called it**: all four paths that build a `PromptParts` assembled the block themselves
// with `anchors.iter().map(|a| a.render()).collect::<Vec<_>>().join("\n")`, which enforces neither.
// Pinned constraints are the one thing the preamble tells the model to rely on across compaction, so
// an anchor set outgrowing its budget was being concatenated without complaint.
//
// That was invisible to the test suite because every test of `render_anchor_block` calls the function
// directly, and the gateway tests only build anchor sets small enough for the `join` to look right.
// A behavioural test would need to pin ten thousand tokens through the tool surface to exercise the
// same path, which is a lot of machinery to assert a wiring.
//
// So this asserts the wiring. It is a grep, and it is honest about being one: a regression here is
// somebody writing the `join` again, and that is exactly what a text match catches.
//
// # What would make this wrong
//
// If a future path builds an anchor block some third way, this check does not see it — it can only
// notice the four call sites losing their caller. That is the narrower of the two failure modes and
// the one that actually happened.
import { readdirSync, readFileSync } from "node:fs";
import { dirname, join, relative } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..", "..");

/** Files that build a prompt for a harness, each of which must route anchors through the check. */
const MUST_ROUTE = [
  "crates/sakur4d/src/tools.rs",
  "crates/sakur4d/src/cli.rs",
  "crates/sakur4d/src/proxy.rs",
  "crates/sakur4-testkit/src/harness.rs",
];

/** The pattern that bypasses the check, and did. */
const BYPASS = /anchors\.iter\(\)\.map\(\|a\| a\.render\(\)\)/g;

// # Exactly one `join` is allowed, and it is in `tools.rs`
//
// The `sakur4://anchors/` *resource* renders the Anchor Set for a reader to look at. There is no
// budget to enforce — it is an inventory, not a prompt — so a `join` is right there, and it is the
// only place. Counting rather than pattern-matching keeps this honest: a heuristic that tried to
// infer "is this the resource?" from surrounding lines flagged the legitimate one on its first run,
// which is how a check starts lying about the code it guards.
const ALLOWED_JOINS = new Map([["crates/sakur4d/src/tools.rs", 1]]);

const problems = [];
for (const rel of MUST_ROUTE) {
  const path = join(ROOT, rel);
  let text;
  try {
    text = readFileSync(path, "utf8");
  } catch {
    problems.push(`${rel}: missing, so it can no longer be checked`);
    continue;
  }
  if (!text.includes("render_anchor_block")) {
    problems.push(`${rel}: does not call \`render_anchor_block\` (FR-4's budget refusal)`);
  }
  // # Comments quote the pattern, and quoting it is not doing it
  //
  // The note in `tools.rs` that explains this defect contains the bypass pattern verbatim, so a
  // naive count found two joins there and reported a problem with the fix that removed them. Counting
  // only lines that are not comments is the difference between a check that reads the code and one
  // that reads the prose about the code.
  const joins = text
    .split(/\r?\n/)
    .filter((line) => !/^\s*(\/\/|\*)/.test(line))
    .filter((line) => BYPASS.test(line)).length;
  const allowed = ALLOWED_JOINS.get(rel) ?? 0;
  if (joins > allowed) {
    problems.push(
      `${rel}: ${joins} hand-built anchor block(s), ${allowed} allowed — those bypass the budget check`,
    );
  }
}

if (problems.length) {
  console.log(`  ${MUST_ROUTE.length} prompt path(s) checked, ${problems.length} problem(s)`);
  for (const p of problems) console.log(`    ${p}`);
  process.exit(1);
}
console.log(`  all ${MUST_ROUTE.length} prompt path(s) route anchors through the budget check`);
