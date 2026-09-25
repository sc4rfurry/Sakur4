#!/usr/bin/env node
// Does the **published** wiki match `wiki/` in the repository?
//
// # Why this check exists
//
// The published wiki is a separate git repository, and nothing compared it with the tracked pages. It drifted
// for several rounds without anybody noticing, and it drifted in the worst possible direction: the
// **Limitations** page still described the pipelined-read ordering defect as **open**, which `v0.2.1` had
// fixed and shipped.
//
// That is the most damaging kind of staleness this project can have. A reader deciding whether to trust
// Sakur4 is told to read the Limitations page first, and it was telling them the headline defect was live when
// it was not. The repository's own documentation guards could not see it: `doc-links.mjs` checks that wiki
// links resolve, and the other guards read the README — **none of them reads the published wiki's prose.**
//
// # What it does, and what it refuses to do
//
// It clones the wiki, compares every page, and reports which differ. **It never pushes.** Publishing is a
// deliberate act with a commit message; a check that silently republished would make the wiki a build
// artifact rather than something a person decided to publish. `wiki/publish.sh` is the way to publish.
//
//   node docs/verification/wiki-published.mjs            # compare, report
//   node docs/verification/wiki-published.mjs --ref main # a different default branch
import { execFileSync, spawnSync } from "node:child_process";
import { existsSync, readdirSync, readFileSync, rmSync } from "node:fs";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..", "..");
const wikiDir = join(ROOT, "wiki");
const REPO = process.env.SAKUR4_REPO || "sc4rfurry/Sakur4";

/** Line endings are normalised: `core.autocrlf` differs per machine and is not drift. */
const normalise = (text) => text.replace(/\r\n/g, "\n").replace(/\s+$/, "");

const clone = mkdtempSync(join(tmpdir(), "sakur4-wiki-"));
let ok = false;
try {
  const cloneResult = spawnSync(
    "git",
    ["clone", "--depth", "1", "--quiet", `https://github.com/${REPO}.wiki.git`, clone],
    { encoding: "utf8", timeout: 120_000 },
  );
  if (cloneResult.error || cloneResult.status !== 0) {
    const detail = (cloneResult.stderr || cloneResult.error?.message || "").trim().split("\n").pop();
    console.log(`  SKIP: the wiki repository could not be cloned (${detail || "no detail"})`);
    // Exit 0 with a skip reason: a network failure is not a documentation defect, and a check that goes red
    // offline is a check people learn to ignore.
    process.exit(0);
  }

  const local = readdirSync(wikiDir).filter((f) => f.endsWith(".md"));
  const missing = [];
  const differing = [];
  const extra = [];

  for (const file of local) {
    const publishedPath = join(clone, file);
    if (!existsSync(publishedPath)) {
      missing.push(file);
      continue;
    }
    const a = normalise(readFileSync(join(wikiDir, file), "utf8"));
    const b = normalise(readFileSync(publishedPath, "utf8"));
    if (a !== b) differing.push(file);
  }

  for (const file of readdirSync(clone).filter((f) => f.endsWith(".md"))) {
    if (!local.includes(file) && file !== "README.md") extra.push(file);
  }

  // # The stale claims that matter, checked by name
  //
  // A diff report is only useful to someone who then reads the diff. These are the two claims whose being
  // stale actually misleads a reader — one said a shipped fix was outstanding, the other said a policy check
  // did not exist — so they are checked directly, and a page that still carries them is called out even if
  // the rest of it happens to match.
  const STALE = [
    { file: "Limitations.md", pattern: /Status: open/i, says: "still calls a defect open" },
    { file: "Security.md", pattern: /cargo deny.*is not run/i, says: "still says cargo deny is not run" },
  ];
  const staleClaims = [];
  for (const { file, pattern, says } of STALE) {
    const path = join(clone, file);
    if (existsSync(path) && pattern.test(readFileSync(path, "utf8"))) {
      staleClaims.push(`${file} ${says}`);
    }
  }

  if (differing.length || missing.length || extra.length || staleClaims.length) {
    console.log(
      `  the published wiki is behind wiki/: ${differing.length} differing, ${missing.length} missing, ${extra.length} extra`,
    );
    for (const f of differing) console.log(`    differs  ${f}`);
    for (const f of missing) console.log(`    missing  ${f}`);
    for (const f of extra) console.log(`    extra    ${f} — published but not in wiki/`);
    for (const s of staleClaims) console.log(`    STALE    ${s}`);
    console.log("  publish with:  ./wiki/publish.sh");
    process.exit(1);
  }

  console.log(`  all ${local.length} page(s) match the published wiki`);
  ok = true;
} finally {
  rmSync(clone, { recursive: true, force: true });
}

process.exit(ok ? 0 : 1);
