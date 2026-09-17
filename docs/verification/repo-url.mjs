#!/usr/bin/env node
/**
 * Check that the repository URL is real everywhere it appears.
 *
 * # Why this is a check and not a note
 *
 * `repository` in `[workspace.package]` ends up in the published manifest of every crate. A
 * placeholder there produces a crate whose repository link 404s — a crate nobody can inspect, and
 * a signal that the author did not finish. It is also invisible in every local check: the build
 * succeeds, the tests pass, and the wrong string ships.
 *
 * This was written after the placeholder sat in sixteen files while everything was green.
 *
 * # What counts as real
 *
 * Any `github.com/<owner>/<repo>` URL, provided the owner is not an obviously placeholder-ish
 * value. The point is not to pin one repository — a fork is legitimate — but to catch the two
 * states that are always wrong: an empty value, and the generic `your-org`/`example`/`sakur4/sakur4`
 * forms that a template leaves behind.
 *
 * Usage:
 *   node docs/verification/repo-url.mjs [--root .]
 */

import { readFileSync, readdirSync, statSync, existsSync } from "node:fs";
import { join, relative, extname } from "node:path";

const argv = process.argv.slice(2);
const arg = (name, fallback) => {
  const i = argv.indexOf(`--${name}`);
  return i >= 0 && argv[i + 1] && !argv[i + 1].startsWith("--") ? argv[i + 1] : fallback;
};

const ROOT = arg("root", process.cwd());
const SKIP = new Set(["target", "node_modules", ".git", "dist"]);
const EXTS = new Set([".toml", ".md", ".rs", ".mjs", ".py", ".sh", ".yml", ".yaml", ".json", ".ts"]);

// Owners that mean "nobody filled this in". Matching is case-insensitive.
//
// # `sakur4` was in this list, and it is the real repository name
//
// The first version listed `sakur4` as a placeholder owner, because the original mistake was
// `sakur4/sakur4` — an organisation nobody owns, standing in for a repository that did not exist
// yet. Once the real one was created the *name* `Sakur4` became correct, and the guard flagged
// twenty legitimate references.
//
// A guard that cannot tell "unfilled template" from "the real thing" is worse than none: it fails
// on correct input, and the fix people reach for is to stop running it. The distinguishing
// property is not the word — it is whether the owner is a person or an organisation. So the check
// is on owners that are obviously nobody, and the manifest value is validated as a URL separately.
const PLACEHOLDERS = ["your-org", "yourorg", "example", "owner", "username", "<owner>", "todo"];

function* walk(dir) {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    if (SKIP.has(entry.name)) continue;
    const full = join(dir, entry.name);
    if (entry.isDirectory()) yield* walk(full);
    else if (entry.isFile() && EXTS.has(extname(entry.name))) yield full;
  }
}

let problems = 0;
let checked = 0;

for (const file of walk(ROOT)) {
  const text = readFileSync(file, "utf8");
  for (const [index, line] of text.split("\n").entries()) {
    // Every github.com owner/repo reference, whether in a badge, a link or a manifest value.
    for (const match of line.matchAll(/github\.com\/([A-Za-z0-9._-]+)\/([A-Za-z0-9._-]+)/g)) {
      checked += 1;
      const owner = match[1].toLowerCase();
      const repo = match[2].replace(/\.git$/, "").toLowerCase();
      if (PLACEHOLDERS.includes(owner) || PLACEHOLDERS.includes(repo)) {
        problems += 1;
        console.log(`  ${relative(ROOT, file)}:${index + 1}  placeholder owner or repo: ${match[0]}`);
        console.log(`      ${line.trim().slice(0, 100)}`);
      }
    }
  }
}

// The manifest value specifically: it is the one that ships to crates.io.
const manifest = join(ROOT, "Cargo.toml");
if (existsSync(manifest)) {
  const text = readFileSync(manifest, "utf8");
  const repository = text.match(/^\s*repository\s*=\s*"([^"]*)"/m)?.[1] ?? "";
  if (!repository) {
    problems += 1;
    console.log("  Cargo.toml  [workspace.package] has no repository value");
  } else if (!/^https:\/\/github\.com\/[A-Za-z0-9._-]+\/[A-Za-z0-9._-]+$/.test(repository)) {
    problems += 1;
    console.log(`  Cargo.toml  repository is not a GitHub URL: ${repository}`);
  } else {
    console.log(`  manifest repository: ${repository}`);
  }
}

console.log("");
if (problems === 0) {
  console.log(`${checked} GitHub reference(s) checked, none are placeholders`);
  process.exit(0);
}
console.log(`${problems} placeholder reference(s) — a published crate would link to a 404`);
process.exit(1);
