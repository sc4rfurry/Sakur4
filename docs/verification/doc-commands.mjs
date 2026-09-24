#!/usr/bin/env node
/**
 * Check that every `sakur4d` command the documentation shows is a command that exists.
 *
 * # Why
 *
 * The README's quick start told a reader to run `sakur4d impact src::auth::validate`, which fails:
 * the name is a placeholder and nothing said so. That is the shape of the problem — a documented
 * command is an instruction, and an instruction that cannot work costs the reader the exact
 * confidence the documentation was supposed to give them.
 *
 * A second instance, found earlier and further away: `SKILL.md` documented `map --names` and the
 * skill's CLI had no such flag. It had been wrong since it was written, and nothing checked it.
 *
 * # What it does
 *
 * Extracts every `sakur4d …` line from fenced `bash` blocks in the documented files, then asks the
 * real CLI:
 *
 *   * the subcommand must appear in `sakur4d help`;
 *   * every `--flag` must appear in that subcommand's `--help`;
 *   * a line containing `<...>` is a template — it is checked for the subcommand and flags but its
 *     positional arguments are not run, because they are meant to be replaced.
 *
 * # What it does not do
 *
 * It does not execute the commands. Running them needs a store, a repository and sometimes a
 * server, and a check that only passes on a configured machine is a check that gets ignored. This
 * verifies the shape of each command, which is the part that goes stale when the CLI changes.
 *
 * Usage:
 *   node docs/verification/doc-commands.mjs [--bin PATH]
 */

import { spawnSync } from "node:child_process";
import { existsSync, readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";

const argv = process.argv.slice(2);
const arg = (name, fallback) => {
  const i = argv.indexOf(`--${name}`);
  return i >= 0 && argv[i + 1] && !argv[i + 1].startsWith("--") ? argv[i + 1] : fallback;
};

const ROOT = process.cwd();
const EXE = process.platform === "win32" ? "sakur4d.exe" : "sakur4d";
const BIN = arg("bin", [
  join(ROOT, "target", "release", EXE),
  join(ROOT, "target", "debug", EXE),
  join(process.env.USERPROFILE ?? process.env.HOME ?? "", ".cargo", "bin", EXE),
].find(existsSync) ?? "");

if (!BIN || !existsSync(BIN)) {
  console.error("  no sakur4d found; build one with `cargo build -p sakur4d`");
  process.exit(2);
}

/**
 * Every markdown file that could show a command, found by walking the tree.
 *
 * # A hard-coded list checked one file out of many
 *
 * The first version named four documents. Only `README.md` had a fenced `bash` block containing a
 * `sakur4d` line, so it reported "18 commands across 1 file" and looked thorough — while
 * `crates/sakur4d/README.md` mentioned `sakur4d` fifteen times and was never read. A list of files
 * to check is a list that goes stale the moment somebody adds a document, and its staleness looks
 * exactly like success.
 *
 * The walk skips the verification directory, because these scripts quote commands in their own
 * usage comments and are not instructions to a user.
 */
function* markdownFiles(dir) {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    if (entry.name === "target" || entry.name === ".git" || entry.name === "node_modules") continue;
    const full = join(dir, entry.name);
    if (entry.isDirectory()) {
      if (entry.name === "verification" && dir.endsWith("docs")) continue;
      yield* markdownFiles(full);
    } else if (entry.name.endsWith(".md")) {
      yield full;
    }
  }
}

/** Strip a trailing shell comment: prose about the command, not part of it. */
const stripComment = (line) => line.replace(/\s+#\s.*$/, "").trim();

const commands = [];
let files = 0;
for (const path of markdownFiles(ROOT)) {
  const text = readFileSync(path, "utf8");
  if (!text.includes("sakur4d")) continue;
  const rel = path.slice(ROOT.length + 1).replaceAll("\\", "/");
  // Fences are labelled `bash`, `sh`, `console` or unlabelled; all can hold a command. A `console`
  // fence prefixes a prompt, which is stripped below.
  let found = false;
  // # `[^\n]*\n` rather than `\r?\n`
  //
  // The fence's language tag and its newline are matched loosely on purpose. An earlier version
  // spelled the line ending as an escape sequence, and a later edit through a shell round-trip
  // turned that sequence into its literal characters — so the pattern stopped matching anything,
  // the script found one command instead of eighteen, and it reported success. Matching "rest of
  // the line, then a newline" accepts LF and CRLF alike and cannot be broken by how the file was
  // last written.
  for (const block of text.matchAll(/```([^\n]*)\n([\s\S]*?)```/g)) {
    // # A `console` fence shows output, not only commands
    //
    // `docs/RELEASING.md` has a `console` block whose first line is the *result* of running the
    // binary — `sakur4d 0.2.0` — and reading it as a command reported `'0.2.0' is not a subcommand`.
    // In a `console` fence a line is a command only when it carries a prompt, so that is what is
    // required here. A `bash` fence is all commands and needs no marker.
    const isConsole = /\bconsole\b/.test(block[1]);
    // # Join line continuations before reading
    //
    // A command written across lines ends each with a backslash, and reading the block line-by-line
    // made the backslash the second token — so `sakur4d --db X \` parsed as a subcommand named `\`,
    // and the check reported a command that cannot run when the command was fine and the parser was
    // not. Shell syntax has to be unwound before it can be read.
    const joined = block[2].replace(/\\\r?\n\s*/g, " ");
    for (const [index, raw] of joined.split("\n").entries()) {
      const trimmed = raw.trimStart();
      if (isConsole && !/^[$#>]\s/.test(trimmed)) continue;
      const stripped = trimmed.replace(/^\s*[$#>]\s?/, "");
      const line = stripComment(stripped);
      if (!/^sakur4d\s/.test(line)) continue;
      // No de-duplication: an earlier version skipped a command already seen anywhere in the same
      // file, which silently dropped repeats and made the reported count an undercount. A command
      // written twice is still two instructions a reader may follow.
      commands.push({ doc: rel, line, where: `${rel}:${index + 1}` });
      found = true;
    }
  }
  if (found) files += 1;
}

if (commands.length === 0) {
  console.log("  no documented sakur4d commands found");
  process.exit(0);
}

// The top-level subcommands, read once from `help`.
const help = spawnSync(BIN, ["help"], { encoding: "utf8" }).stdout ?? "";
const subcommands = new Set(
  [...help.matchAll(/^\s{2}([a-z][a-z0-9-]+)\s/gm)].map((m) => m[1]),
);

/** Flags accepted by a subcommand, or `null` when its help could not be read. */
const flagCache = new Map();
function flagsFor(sub) {
  if (!sub) return null;
  if (flagCache.has(sub)) return flagCache.get(sub);
  const out = spawnSync(BIN, [sub, "--help"], { encoding: "utf8" }).stdout ?? "";
  const flags = new Set([...out.matchAll(/(--[a-z][a-z0-9-]*)/g)].map((m) => m[1]));
  // `--help` is accepted everywhere and may not be listed.
  flags.add("--help");
  flagCache.set(sub, flags);
  return flags;
}

let problems = 0;
let checked = 0;

for (const { line, where } of commands) {
  // # Find the subcommand by name, not by position
  //
  // The first version walked left to right and treated any leading `--flag` as global, consuming
  // the next token as its value. So `sakur4d --budget 1500 repo-map --made-up-flag` read `--budget`
  // as a global, swallowed `repo-map` as its argument, found no subcommand, and skipped the line
  // entirely — reporting success on a command whose flag does not exist. It passed the test written
  // to prove it worked, which is the most useful thing that happened to it.
  //
  // clap accepts flags on either side of the subcommand, so position says nothing. The subcommand
  // is the first bare token that *is* a subcommand; everything else is an argument to it.
  const tokens = line.split(/\s+/).slice(1);

  // # Walk the arguments, skipping the value of any flag that takes one
  //
  // The subcommand is the first bare token that is not consumed as a flag's argument. Earlier
  // versions got this wrong in both directions: treating every bare token as the subcommand flagged
  // `/tmp/hermes.db` in `--db /tmp/hermes.db`, and searching only for a *known* subcommand made a
  // typo invisible. Both were found by running the check against a document it had just passed.
  let sub = null;
  let subIndex = -1;
  for (let i = 0; i < tokens.length; i += 1) {
    const token = tokens[i];
    if (token.startsWith("--")) {
      // `--flag=value` is self-contained; `--flag value` consumes the next token.
      if (!token.includes("=") && tokens[i + 1] !== undefined && !tokens[i + 1].startsWith("-")) {
        i += 1;
      }
      continue;
    }
    if (token.startsWith("-")) continue; // a short flag, or a negative number
    sub = token;
    subIndex = i;
    break;
  }

  if (sub === null) {
    // Only flags: `sakur4d --help`, `sakur4d --version`. The top-level help lists those.
    continue;
  }

  const args = [...tokens.slice(0, subIndex), ...tokens.slice(subIndex + 1)];
  checked += 1;

  // # An unknown subcommand is a problem, not a reason to skip
  //
  // An earlier version searched for the first bare token that *was* a known subcommand, and quietly
  // skipped the line when none matched. Renaming `demo` to `demostrate` in a document therefore
  // reduced the checked count from 18 to 17 and reported success — the check declared a typo valid
  // by declining to look at it. A check that passes by finding nothing is the failure mode this
  // whole directory exists to avoid.
  if (!subcommands.has(sub)) {
    problems += 1;
    console.log(`  ${where}: '${sub}' is not a subcommand`);
    console.log(`      ${line}`);
    continue;
  }

  const flags = flagsFor(sub);
  if (flags === null) continue;

  // Global flags clap accepts on every subcommand and does not repeat in each subcommand's help.
  const GLOBAL_FLAGS = new Set([
    "--db",
    "--backend",
    "--context-window",
    "--profile",
    "--names",
    "--log",
    "--no-dream",
    "--banner",
    "--help",
    "--version",
  ]);

  for (const token of args) {
    if (!token.startsWith("--")) continue;
    const bare = token.split("=")[0];
    if (GLOBAL_FLAGS.has(bare)) continue;
    if (!flags.has(bare)) {
      problems += 1;
      console.log(`  ${where}: '${sub}' has no ${bare}`);
      console.log(`      ${line}`);
    }
  }
}

console.log("");
console.log(`  ${checked} documented command(s) across ${files} file(s)`);
console.log("");
if (problems === 0) {
  console.log("every documented command names a real subcommand and real flags");
  process.exit(0);
}
console.log(`${problems} documented command(s) cannot run as written`);
process.exit(1);
