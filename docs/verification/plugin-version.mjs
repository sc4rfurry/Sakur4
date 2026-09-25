#!/usr/bin/env node
// Does every place that states the OMP plugin's version state the same one?
//
// # Why this exists
//
// The plugin's version lives in four places and only one of them is a manifest:
//
//   * `integrations/omp-plugin/package.json` — the version
//   * `integrations/omp-plugin/index.ts` — `clientInfo` in **three** handshake sites
//
// Bumping `package.json` alone leaves the other three behind, and nothing noticed: after the `0.2.1`
// release the package said `0.2.1` while every handshake the plugin performed still announced `0.1.0`.
// That is a version that appears in a daemon's `sessions` table and in a log, so it is not cosmetic — it is
// the field someone reads to answer "which plugin is this?"
//
// `docs/RELEASING.md` gained a table of files the version number does not lead you to, and this was not on
// it. A checklist is the thing that drifts; a check is not.
//
// It reads the sources rather than the built output: `install.mjs` copies `index.ts` and the plugin is
// compiled by OMP, so the source is the contract.
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..", "..");
const pluginDir = join(ROOT, "integrations", "omp-plugin");

const declared = JSON.parse(readFileSync(join(pluginDir, "package.json"), "utf8")).version;

// Every site that announces a version to the daemon. Named rather than pattern-matched so that adding a
// fourth site is a deliberate edit here — the failure this guards against is a site nobody remembered.
const SOURCES = ["index.ts"];
const ANNOUNCED = /clientInfo:\s*\{\s*name:\s*"omp-sakur4",\s*version:\s*"([^"]+)"\s*\}/g;

const mismatches = [];
let sites = 0;

for (const file of SOURCES) {
  const text = readFileSync(join(pluginDir, file), "utf8");
  for (const match of text.matchAll(ANNOUNCED)) {
    sites += 1;
    if (match[1] !== declared) {
      const line = text.slice(0, match.index).split("\n").length;
      mismatches.push(`${file}:${line} announces ${match[1]}, package.json says ${declared}`);
    }
  }
}

if (sites === 0) {
  // # A check that finds nothing to check is a check that cannot fail
  //
  // If the pattern stops matching — a rename, a refactor to a shared constant — this would report success
  // having verified nothing, which is the defect this project has recorded four times now. So finding zero
  // handshake sites is a failure with instructions rather than a pass.
  console.log(
    `  no \`clientInfo\` handshake site matched in ${SOURCES.join(", ")} — the pattern is stale, not the plugin`,
  );
  console.log("  expected: clientInfo: { name: \"omp-sakur4\", version: \"…\" }");
  process.exit(1);
}

if (mismatches.length) {
  console.log(`  the OMP plugin announces a version that is not its own (${declared}):`);
  for (const m of mismatches) console.log(`    ${m}`);
  process.exit(1);
}

console.log(`  the OMP plugin's ${sites} handshake site(s) and package.json all say ${declared}`);
