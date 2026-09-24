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
    // # Other checkouts are not this repository
    //
    // This walked into `.kilo/worktrees/…`, a local agent worktree holding a full second copy of the
    // tree, and reported a broken link in that copy's `SECURITY.md`. Two things were wrong with
    // counting it: the file is not part of this repository, and it is untracked, so the check's result
    // depended on what happened to be on the machine — the same class of mistake as trusting a single
    // sample.
    //
    // Anything hidden is skipped rather than named one by one, because the next tool that creates a
    // directory will not be on this list either.
    if (entry.name.startsWith(".") || ["target", "node_modules"].includes(entry.name)) continue;
    const path = join(dir, entry.name);
    if (entry.isDirectory()) markdownFiles(path, out);
    else if (entry.name.endsWith(".md")) out.push(path);
  }
  return out;
}

const broken = [];
const skipped = [];
let checked = 0;
let wikiChecked = 0;

// # GitHub Wiki links are filenames, not paths
//
// `[Limitations](Limitations)` is how a wiki page links to another wiki page, and it resolves to
// `Limitations.md` on github.com. It is not a filesystem path, so resolving it against the directory —
// which is right for every other file in this repository — reported 109 broken links the moment `wiki/`
// existed, every one of them a page that is present and correct.
//
// So wiki pages are checked against their **siblings**: a link is valid if `<name>.md` exists beside
// the page. Links that are not page links — a URL, an anchor — are skipped as everywhere else, and a
// link out of the wiki is left alone because the wiki is published as its own repository and its
// `../` means something different there.
const isWiki = (file) => relative(ROOT, file).startsWith(`wiki${sep}`);

for (const file of markdownFiles(ROOT)) {
  const text = readFileSync(file, "utf8");
  const wiki = isWiki(file);
  for (const match of text.matchAll(/\]\(([^)\s]+)\)/g)) {
    const link = match[1];
    if (/^(https?:|mailto:|#|tel:)/.test(link)) continue;
    const target = link.split("#")[0];
    if (!target) continue;

    if (wiki) {
      // A wiki link is a page name; anything with a path separator is not one.
      if (target.includes("/")) continue;
      wikiChecked += 1;
      if (!existsSync(join(dirname(file), `${target}.md`))) {
        broken.push(`${relative(ROOT, file)} -> ${link} (no such wiki page)`);
      }
      continue;
    }

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
  console.log(`  ${checked} file link(s) and ${wikiChecked} wiki link(s) checked, ${broken.length} broken`);
  for (const b of broken.slice(0, 20)) console.log(`    ${b}`);
  if (broken.length > 20) console.log(`    … and ${broken.length - 20} more`);
  process.exit(1);
}
console.log(
  `  ${checked} file link(s) and ${wikiChecked} wiki link(s) resolve` +
    (skipped.length ? ` (${skipped.length} left the repository and were not checked)` : ""),
);
