#!/usr/bin/env node
// The wiki's own consistency: every page exists, every link resolves, every page has a footer.
//
// # Why the wiki gets its own check
//
// The pages are published to a *different* repository, so nothing in the normal verification sees them:
// `docs/verification/doc-links.mjs` resolves filesystem-relative links, and a wiki link like
// `[Limitations](Limitations)` is neither a file path nor a URL. A typo there renders as a red link on
// the published wiki, which is visible but not loud, and the sidebar is the first thing a reader clicks.
//
// So this checks the three things that break silently:
//
//   * a link to a page that does not exist — including in `_Sidebar.md` and `_Footer.md`;
//   * a page with no footer, which looks unfinished next to the others;
//   * a page that nothing links to, which is a page nobody will find.
import { readdirSync, readFileSync } from "node:fs";
import { dirname, join, basename } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..", "..");
const wikiDir = join(ROOT, "wiki");

const files = readdirSync(wikiDir).filter((f) => f.endsWith(".md"));
// `README.md` is publishing instructions for a maintainer, not a wiki page.
const pages = files.filter((f) => f !== "README.md" && !f.startsWith("_")).map((f) => basename(f, ".md"));
const special = files.filter((f) => f.startsWith("_"));
const pageSet = new Set(pages);

const problems = [];
let links = 0;

for (const file of files) {
  const text = readFileSync(join(wikiDir, file), "utf8");

  // Every `[text](Target)` whose target is not a URL and not an anchor is a wiki page link.
  for (const m of text.matchAll(/\]\(([^)\s]+)\)/g)) {
    const target = m[1];
    if (/^(https?:|mailto:|#)/.test(target)) continue;
    const page = target.split("#")[0];
    if (!page) continue;
    links += 1;
    if (!pageSet.has(page)) {
      problems.push(`${file}: links to \`${page}\`, which is not a page`);
    }
  }

  // Pages need the footer; `_Sidebar`/`_Footer` are chrome.
  if (!file.startsWith("_") && file !== "README.md" && !text.includes("<sub>")) {
    problems.push(`${file}: no <sub> footer — it will look unfinished beside the others`);
  }
}

// # Wiki pages name commands, and a page I wrote named one that does not exist
//
// `Home.md` said `sakur4d config omp   # Oh My Pi`. There is no `omp` harness — `config` accepts
// `hermes`, `claude`, `claude-code`, `generic-http`, `generic-stdio`, and Oh My Pi is a native
// extension installed by a Node script instead. The link checker passed the page, because the command
// was not a link.
//
// `doc-commands.mjs` already does this for `README.md`, against a built binary. The wiki is published
// to a *different* repository and was covered by nothing, so it gets the same treatment: every
// `sakur4d <word>` in a code span or fence must name a real subcommand.
//
// Only code spans and fences are read. Prose like "the sakur4d binary" or "point sakur4d at it" is not
// a command, and a check that flags it is a check that gets ignored.
const SUBCOMMANDS = new Set([
  "serve", "proxy", "config", "doctor", "gen-key", "index", "repo-map", "impact", "symbol",
  "recall", "commit", "pin", "anchors", "plan", "snapshot", "restore", "receipt", "dream",
  "staleness", "demo", "help",
]);

/** Fenced code and inline code spans, which is where a command actually appears. */
function commandMentions(text) {
  const out = [];
  for (const f of text.matchAll(/```[a-z]*\n([\s\S]*?)```/g)) {
    // # A comment inside a code block is not a command
    //
    // `# wait for the registry to index it — publishing sakur4d too early fails to resolve the
    // dependency` reads as `sakur4d too`, and a prose line in a block that *explains* that a command
    // does not exist reads as that command. Both were flagged against pages that are correct.
    //
    // Shell comments are stripped, and lines that are plainly prose — starting with `**` or a digit
    // followed by `.` — are skipped. That last one is for the Troubleshooting page, which says
    // "**There is no `sakur4d status` subcommand**" and has to be able to.
    const body = f[1]
      .split(/\r?\n/)
      .map((line) => line.replace(/(^|\s)#.*$/, ""))
      .filter((line) => !/^\s*(\*\*|\d+\.|\|)/.test(line))
      .join("\n");
    out.push(body);
  }
  for (const s of text.matchAll(/`([^`\n]+)`/g)) {
    if (/^\s*(\*\*|\d+\.)/.test(s[1])) continue;
    out.push(s[1]);
  }
  return out;
}

// # The subcommand is simply the token after the binary name
//
// Two versions of this were wrong in opposite directions and both are worth recording, because the
// second is the dangerous kind.
//
// The first searched the whole page for `sakur4d <word>` and flagged "the `sakur4d binary`", "point
// `sakur4d` at it", "`sakur4d package.json`" — sentences read as commands. Fourteen findings, mostly
// noise.
//
// The second tried to tell a command line from prose by requiring the line to *begin* with the binary.
// That silenced the noise and also silenced everything else: it reported zero problems, including for a
// deliberately impossible `sakur4d nonexistent-cmd`, because almost every real mention here is an
// inline span mid-sentence. A check that cannot fail is worse than no check — it reports a clean pass
// and gets trusted.
//
// So: read only fenced blocks and inline spans, and take the token immediately after `sakur4d`. Prose
// that says "the sakur4d binary" yields `binary`, which is not a subcommand — so the word list excludes
// the handful of nouns that can follow the binary name in English. That is a small, explicit allowance
// rather than a heuristic, and it is checked against a known-bad command below.
// # Words that follow the binary's name in English, or on the next line of a block
//
// `binary`, `process`, `daemon`, `on`, `at` — prose. `node` is the one that keeps coming back: a block
// that says "install the OMP plugin" and then `node integrations/omp-plugin/install.mjs` puts `node`
// two lines after a `sakur4d`, and `status` appears when a page explains that `sakur4d status` is *not*
// a command — a page has to be able to say that.
//
// This is a small, explicit allowance rather than a heuristic. Before it, the check listed five
// problems and every one was a page being correct.
const NOT_A_COMMAND = new Set([
  "binary", "process", "daemon", "server", "itself", "on", "is", "at", "too", "node", "status", "exe",
]);

for (const file of files) {
  if (file === "README.md") continue;
  const text = readFileSync(join(wikiDir, file), "utf8");
  for (const chunk of commandMentions(text)) {
    for (const m of chunk.matchAll(/sakur4d(?:\.exe)?\s+([a-z][a-z-]+)/g)) {
      const word = m[1];
      if (NOT_A_COMMAND.has(word)) continue;
      if (!SUBCOMMANDS.has(word)) {
        problems.push(`${file}: \`sakur4d ${word}\` is not a subcommand`);
      }
    }
  }
}

// # And the check checks itself
//
// After two versions that did not fire, an assertion is the only way to know this one does. A wiki page
// that names a command no binary has is the defect this exists for; if this cannot see one, it is
// reporting a clean pass it has not earned.
const PROBE = "```bash\nsakur4d definitely-not-a-subcommand\n```";
if (!/definitely-not-a-subcommand/.test(commandMentions(PROBE).join("\n"))) {
  console.log("  THE COMMAND CHECK IS BROKEN: it cannot see a command in a fenced block");
  process.exit(1);
}
const probeWord = commandMentions(PROBE)[0].match(/sakur4d\s+([a-z-]+)/)[1];
if (SUBCOMMANDS.has(probeWord)) {
  console.log("  THE COMMAND CHECK IS BROKEN: its subcommand list accepts anything");
  process.exit(1);
}

// Reachability: every page should be linked from somewhere.
const linked = new Set();
for (const file of files) {
  const text = readFileSync(join(wikiDir, file), "utf8");
  for (const m of text.matchAll(/\]\(([^)\s]+)\)/g)) {
    const page = m[1].split("#")[0];
    if (!/^(https?:|mailto:|#)/.test(m[1]) && page) linked.add(page);
  }
}
for (const page of pages) {
  if (!linked.has(page)) problems.push(`${page}.md: nothing links to it`);
}

if (problems.length) {
  console.log(`  ${pages.length} page(s), ${links} link(s), ${problems.length} problem(s)`);
  for (const p of problems) console.log(`    ${p}`);
  process.exit(1);
}
console.log(
  `  ${pages.length} page(s) + ${special.length} chrome file(s), ${links} link(s), all reachable`,
);
