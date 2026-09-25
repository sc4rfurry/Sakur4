#!/usr/bin/env node
// Two projects with the same directory name must not share a session.
//
// # What this guards
//
// The OMP plugin derived its session id as `omp-${basename(cwd)}` — a directory name. Two projects whose
// directories are both called `api` therefore shared one.
//
// **That was worse than a collision of labels.** `MemoryFabric::session_episodes` filters on `session_id`
// alone — not on `project_id` — and every episodic read goes through it: `timeline` for the assembled prompt,
// `recent_episodes` for the receipt, and the fold and anchor queries beside them. A shared basename meant a
// **shared transcript**, so the timeline built for one project could contain the other's turns. Recall was
// never affected because it is project-scoped; the prompt's own history was.
//
// # Why the function is extracted rather than imported
//
// `index.ts` imports `ExtensionAPI` and friends from `@earendil-works/pi-coding-agent`, which is not
// installed here and is OMP's own package. Importing the module would need it. **Extracting the function's
// source and evaluating it has a second benefit that matters more:** if the function is renamed, moved or
// reshaped, this fails loudly instead of silently testing nothing — the defect this project has recorded four
// times.
import { spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..", "..");
const source = readFileSync(join(ROOT, "integrations", "omp-plugin", "index.ts"), "utf8");

// The exported function, from `export function sessionIdFor` to its closing brace at column zero.
const start = source.indexOf("export function sessionIdFor");
if (start < 0) {
  console.log("  FAIL  `sessionIdFor` is gone from index.ts — this check would otherwise test nothing");
  process.exit(1);
}
const end = source.indexOf("\n}", start);
if (end < 0) {
  console.log("  FAIL  the body of `sessionIdFor` could not be delimited");
  process.exit(1);
}
const body = source.slice(start, end + 2);

// `resolve` and `basename` come from `node:path`, `createHash` from `node:crypto` — the same two the module
// imports, so this evaluates the real implementation rather than a copy of it.
//
// The carrier is a `.ts` file rather than `.mjs` so that Node's own **type stripping** removes the
// annotations. Rewriting them out with a regex would mean testing a mangled copy of the function, which is
// exactly the kind of check-that-tests-the-wrong-thing this project keeps finding.
const carrier = mkdtempSync(join(tmpdir(), "sakur4-session-"));
const modulePath = join(carrier, "session.ts");
writeFileSync(
  modulePath,
  `import { createHash } from "node:crypto";
import { resolve } from "node:path";
function basename(path: string): string {
  const parts = path.split(/[\\\\/]/).filter(Boolean);
  return parts.pop() ?? "default";
}
${body}

`,
);

const probe = `
import { sessionIdFor } from ${JSON.stringify(pathToFileURL(modulePath).href)};
const out = {
  a: sessionIdFor("/work/one/api"),
  b: sessionIdFor("/work/two/api"),
  aAgain: sessionIdFor("/work/one/api"),
  dotted: sessionIdFor("/work/one/api/../api"),
  nested: sessionIdFor("/work/one/api/src"),
};
process.stdout.write(JSON.stringify(out));
`;

let result;
try {
  result = spawnSync(
    process.execPath,
    ["--experimental-strip-types", "--input-type=module", "-e", probe],
    { encoding: "utf8", timeout: 30_000 },
  );
} finally {
  rmSync(carrier, { recursive: true, force: true });
}

if (result.error || result.status !== 0) {
  console.log(`  FAIL  the extracted function could not run: ${result.stderr?.trim() || result.error}`);
  process.exit(1);
}

const out = JSON.parse(result.stdout);
const failures = [];
const check = (name, ok, detail) => {
  console.log(`  ${ok ? "ok  " : "FAIL"}  ${name}${detail ? ` — ${detail}` : ""}`);
  if (!ok) failures.push(name);
};

// The defect: two projects whose directories share a name.
check(
  "different projects with the same directory name get different sessions",
  out.a !== out.b,
  `${out.a} vs ${out.b}`,
);
// Stability: the same project must keep its memory across runs, or every session starts empty.
check("the same project gets the same session every time", out.a === out.aAgain, out.a);
// `resolve` rather than the raw `cwd`, so `.` and an absolute path to one directory agree.
check("a path with `..` resolves to the same session", out.a === out.dotted, out.dotted);
// Readability, which is why the digest was added to the name rather than replacing it.
check("the id still names the directory", out.a.startsWith("omp-api-"), out.a);
// A subdirectory is a different project root, and must be separable.
check("a nested directory is a different session", out.a !== out.nested, out.nested);

if (failures.length) {
  console.log(`  ${failures.length} check(s) failed`);
  process.exit(1);
}
console.log("  session ids are stable per project and distinct between projects");
