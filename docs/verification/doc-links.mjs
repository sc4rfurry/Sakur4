#!/usr/bin/env node
// Every relative link in the documentation points at something that exists.
//
// # Why this was written
//
// While reading `docs/DESIGN.md` I saw `[bench/](bench/)` and checked whether `bench/` existed at the
// repository root. It does not — the benchmark is at `docs/bench/`. That looked like a dead link in
// two places, and it is not: a relative link resolves against **the file that contains it**, and
// DESIGN.md lives in `docs/`, so `bench/` is `docs/bench/` and correct.
//
// The wrong conclusion is the reason this script exists. "This file does not exist" is a claim worth
// checking rather than acting on, and the check is mechanical — so it belongs in a script instead of
// in my head, where it produced a false alarm in the same round I was congratulating myself on
// checking things.
//
// # What counts
//
// Links that are files in this repository. Absolute URLs are somebody else's problem; a link written
// for a hosting provider's web interface — `SECURITY.md`'s `../../security/advisories/new`, which
// only means anything on github.com — cannot be resolved locally, so links that climb out of the
// repository root are reported as skipped rather than broken. That distinction matters: calling it
// broken would be the same category of mistake as the one above.
import { existsSync, readdirSync, readFileSync, statSync } from "node:fs";
import { dirname, join, relative, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..", "..");

function markdownFiles(dir, out = []) {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    if (["target", "node_modules", ".git"].includes(entry.name)) continue;
    const path = join(dir, entry.name);
    if (entry.isDirectory()) markdownFiles(path, out);
    else if (entry.name.endsWith(".md")) out.push(path);
  }
  return out;
}

const broken = [];
const skipped = [];
let checked = 0;

for (const file of markdownFiles(ROOT)) {
  const text = readFileSync(file, "utf8");
  for (const match of text.matchAll(/\]\(([^)\s]+)\)/g)) {
    const link = match[1];
    if (/^(https?:|mailto:|#|tel:)/.test(link)) continue;
    const target = link.split("#")[0];
    if (!target) continue;

    const resolved = resolve(dirname(file), target);
    checked += 1;

    // A path that leaves the repository cannot be a local file; those are web-only links.
    if (!resolved.startsWith(ROOT + sep)) {
      skipped.push(`${relative(ROOT, file)} -> ${link}`);
      continue;
    }
    if (!existsSync(resolved)) {
      broken.push(`${relative(ROOT, file)} -> ${link}`);
      continue;
    }
    // A directory link is valid markdown; a file link must be a file.
    const stat = statSync(resolved);
    if (stat.isDirectory() && !existsSync(join(resolved, "README.md"))) {
      // Still fine: linking to a directory is meaningful to a reader on a hosting provider.
      continue;
    }
  }
}

if (broken.length) {
  console.log(`  ${checked} relative link(s) checked, ${broken.length} broken`);
  for (const b of broken) console.log(`    ${b}`);
  process.exit(1);
}
console.log(
  `  all ${checked} relative link(s) across the docs resolve` +
    (skipped.length ? ` (${skipped.length} left the repository and were not checked)` : ""),
);
