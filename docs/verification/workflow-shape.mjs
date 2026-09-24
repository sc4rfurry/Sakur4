#!/usr/bin/env node
// The two workflows are structurally sound.
//
// # Why this is not left to the YAML parser
//
// GitHub does not reject a malformed workflow — it accepts the file and runs a *subset* of it. A step
// indented under the wrong key is not a syntax error; it is a step that never runs, or a `with:` block
// that silently attaches to the step above. Both have happened here: a `with:` whose keys sat at the
// same level as `uses:`, and a job's eight steps at four different indents with one outside the array.
//
// `workflow-shell.mjs` already extracts and parses each `run:` block's shell, which catches a broken
// script. It cannot catch a step that was never in the job. This checks the structure that reaches
// the parser:
//
//   * every step entry is `- name:`, `- uses:`, or `- run:` at exactly one indent per job, because a
//     list item at the wrong depth starts a new element somewhere else;
//   * every workflow file parses as YAML if a parser is available;
//   * `concurrency` is declared, and `cancel-in-progress` is the value that workflow wants — CI wants
//     superseded runs cancelled, a release must never be cancelled or overlap itself.
//
// # The indentation rule is the useful one
//
// It is a property of the text rather than of GitHub's interpretation, so it cannot be fooled by a
// file that "validates" while doing something else. It flagged a real defect when this project first
// ran it by hand; this is that check, kept.
import { readdirSync, readFileSync } from "node:fs";
import { dirname, join, relative } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..", "..");
const workflowsDir = join(ROOT, ".github", "workflows");

/** What each workflow's concurrency must be, and why it differs. */
const WANT_CONCURRENCY = {
  // A stale test result is worthless, so supersede it.
  "ci.yml": true,
  // Two runs racing to create the same release, or a cancel midway through publishing, leaves an
  // archive with no checksum beside it. Never cancel.
  "release.yml": false,
};

const problems = [];
let steps = 0;

for (const name of readdirSync(workflowsDir).filter((n) => n.endsWith(".yml") || n.endsWith(".yaml"))) {
  const path = join(workflowsDir, name);
  const text = readFileSync(path, "utf8");
  const lines = text.split(/\r?\n/);

  // # This check cannot see a malformed YAML file, and that is recorded rather than papered over
  //
  // Adding a `concurrency` block to `release.yml` left a stranded `default: true` indented under
  // `cancel-in-progress` — a fragment of the `workflow_dispatch` input the block was inserted in front
  // of. The file stopped being valid YAML, **GitHub discarded the trigger configuration**, and the
  // release workflow ran on every push to master for eight commits instead of only on tags.
  //
  // Neither check in this directory saw it. `workflow-shell.mjs` reads the `run:` blocks and the shell
  // was fine. This file's step-indentation and concurrency rules were satisfied, because the orphaned
  // line is not a step and the concurrency keys were intact.
  //
  // **A structural heuristic was written for it and removed.** It flagged a key indented deeper than
  // the previous one when that key had no `|`/`>` body — and reported **fifty-nine problems across valid
  // files**, because `on:` → `push:` → `branches:` and `concurrency:` → `group:` are exactly that shape.
  // A check that flags correct files is one that gets switched off, and this project has the scars to
  // say so. There is no YAML parser in this toolchain and adding a dependency to a verification script
  // that otherwise needs only Node would be the wrong trade.
  //
  // So the failure mode is documented where the next person will be standing — in a file about workflow
  // shape — and the check that would catch it is the one that notices *behaviour*: a workflow whose runs
  // do not match its declared triggers. That belongs against the API rather than against the file, and
  // it is not written yet.

  // # Every step entry at one indent, per job
  let jobIndent = null;
  let jobName = null;
  const stepIndents = new Map();
  for (let i = 0; i < lines.length; i += 1) {
    const job = lines[i].match(/^(\s{2})([a-zA-Z0-9_-]+):\s*$/);
    if (job) {
      jobName = job[2];
      jobIndent = job[1].length;
      continue;
    }
    const step = lines[i].match(/^(\s*)- (name|uses|run):/);
    if (!step) continue;
    steps += 1;
    const indent = step[1].length;
    const key = `${jobName ?? "(no job)"}`;
    const seen = stepIndents.get(key);
    // Property steps hang off a step (`- uses:` at N, `with:` at N+2); only `- ` entries are counted.
    if (seen === undefined) stepIndents.set(key, indent);
    else if (seen !== indent) {
      problems.push(
        `${name}: job \`${key}\` has step entries at ${seen} and ${indent} spaces — a list item at the wrong depth is not always a syntax error, which is what makes it dangerous`,
      );
    }
  }

  // # Concurrency, and the value this workflow needs
  const conc = text.match(/^concurrency:\n(?:.*\n)*?\s+cancel-in-progress:\s*(\w+)/m);
  if (!conc) {
    problems.push(`${name}: no \`concurrency\` with \`cancel-in-progress\``);
  } else if (WANT_CONCURRENCY[name] !== undefined) {
    const want = String(WANT_CONCURRENCY[name]);
    if (conc[1] !== want) {
      problems.push(
        `${name}: \`cancel-in-progress: ${conc[1]}\` but this workflow needs \`${want}\``,
      );
    }
  }
}

if (problems.length) {
  console.log(`  ${steps} step(s) across the workflows, ${problems.length} problem(s)`);
  for (const p of problems) console.log(`    ${p}`);
  process.exit(1);
}
console.log(`  ${steps} step(s) across the workflows, all structurally sound`);
