#!/usr/bin/env node
/**
 * Check that every `run:` block in a GitHub workflow is valid shell.
 *
 * # Why this exists
 *
 * Twice now a workflow has been committed with a shell syntax error that no local check could
 * see. The first was a stray `done` closing a loop that did not exist, which made the step die
 * before its first statement — the failure surfaced as `syntax error near unexpected token`, at
 * the bottom of a CI log, on a repository whose local suite was green.
 *
 * The second was worse and quieter: two lines lost their indentation, so they stopped being part
 * of the `run:` block at all. YAML accepted it, the step ran, and the shell saw a `for` with no
 * preceding `missing=0` — a variable that would then have been empty rather than zero.
 *
 * Neither is visible by reading the YAML, and neither is caught by `cargo`. They are caught by
 * handing each block to a shell and asking.
 *
 * # What it runs
 *
 * `sh -n` on each block in isolation, which parses without executing. Blocks are extracted by
 * indentation, which is how YAML defines them: a block ends at the first line indented no further
 * than the `run:` key.
 *
 * Usage:
 *   node docs/verification/workflow-shell.mjs [--root .] [--sh PATH]
 */

import { readFileSync, readdirSync, existsSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { join, relative } from "node:path";

const argv = process.argv.slice(2);
const arg = (name, fallback) => {
  const i = argv.indexOf(`--${name}`);
  return i >= 0 && argv[i + 1] && !argv[i + 1].startsWith("--") ? argv[i + 1] : fallback;
};

const ROOT = arg("root", process.cwd());

/** A shell to parse with. Git for Windows ships one; so does every CI runner. */
function findShell() {
  const explicit = arg("sh", process.env.SAKUR4_SH);
  if (explicit && existsSync(explicit)) return explicit;
  const candidates = [
    "C:/Program Files/Git/bin/sh.exe",
    "C:/Program Files/Git/usr/bin/sh.exe",
    "/bin/sh",
    "/usr/bin/sh",
    "sh",
  ];
  for (const candidate of candidates) {
    const probe = spawnSync(candidate, ["-c", "exit 0"], { encoding: "utf8" });
    if (!probe.error && probe.status === 0) return candidate;
  }
  return null;
}

const shell = findShell();
if (!shell) {
  console.log("  no POSIX shell available; cannot check workflow blocks");
  process.exit(2);
}

const workflowDir = join(ROOT, ".github", "workflows");
if (!existsSync(workflowDir)) {
  console.log("  no .github/workflows directory");
  process.exit(0);
}

let blocks = 0;
let invalid = 0;

for (const name of readdirSync(workflowDir).filter((f) => f.endsWith(".yml") || f.endsWith(".yaml"))) {
  const file = join(workflowDir, name);
  const lines = readFileSync(file, "utf8").split(/\r?\n/);
  let index = 0;
  let count = 0;

  while (index < lines.length) {
    const match = lines[index].match(/^(\s+)run: \|\s*$/);
    if (!match) {
      index += 1;
      continue;
    }
    const indent = match[1].length;
    const startLine = index + 2; // 1-based, and the body begins on the next line

    // # Only POSIX-shell blocks
    //
    // A step can declare `shell: pwsh`, and handing PowerShell to `sh -n` reports a syntax error
    // that is purely an artefact of the wrong interpreter — the first version of this check did
    // exactly that and flagged a correct PowerShell block. The declaration is on one of the few
    // lines above the block, so it is read from there.
    let declaredShell = null;
    for (let back = index - 1; back >= 0 && back > index - 6; back -= 1) {
      const shell = lines[back].match(/^\s+shell:\s*(\S+)/);
      if (shell) {
        declaredShell = shell[1].toLowerCase();
        break;
      }
      if (/^\s+- (name|uses|if|id):/.test(lines[back])) break;
    }
    if (declaredShell && !/^(sh|bash)$/.test(declaredShell)) {
      index += 1;
      while (
        index < lines.length &&
        (lines[index].trim() === "" || lines[index].search(/\S/) > indent)
      ) {
        index += 1;
      }
      count += 1;
      blocks += 1;
      console.log(`  ${relative(ROOT, file)}:${startLine}  skipped (shell: ${declaredShell})`);
      continue;
    }

    const body = [];
    index += 1;
    while (
      index < lines.length &&
      (lines[index].trim() === "" || lines[index].search(/\S/) > indent)
    ) {
      body.push(lines[index]);
      index += 1;
    }
    count += 1;
    blocks += 1;

    const parsed = spawnSync(shell, ["-n"], { input: body.join("\n"), encoding: "utf8" });
    if (parsed.status !== 0) {
      invalid += 1;
      const message = (parsed.stderr ?? "").split("\n").filter(Boolean)[0] ?? "unknown";
      console.log(`  ${relative(ROOT, file)}:${startLine}  ${message.replace(/^\/\S+: /, "")}`);
    }
  }
  console.log(`  ${relative(ROOT, file)}  ${count} run block(s)`);
}

console.log("");
if (invalid === 0) {
  console.log(`${blocks} run block(s) parse as shell`);
  process.exit(0);
}
console.log(`${invalid} of ${blocks} run block(s) do not parse — the step would fail before its first statement`);
process.exit(1);
