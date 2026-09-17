#!/usr/bin/env node
/**
 * Remove named sessions from a Sakur4 store.
 *
 * # Why this exists
 *
 * Verification scripts wrote their fixtures into the *default* store — the one a person's real
 * sessions use — because they invoked the CLI without `--db`. That is fixed in the scripts, but
 * the rows they left behind are still there, mixed in with real memory, and finding them by hand
 * across `episodic_stream`, `anchor_set`, `semantic_atlas`, `dependency_graph_edge` and a few
 * FTS shadows is exactly the kind of thing a script should do once and correctly.
 *
 * # What it refuses to do
 *
 * It deletes only the session ids it is given, on the command line, one at a time. There is no
 * `--all`, no pattern, and no default. A tool that can empty a memory store by accident is worse
 * than the mess it cleans.
 *
 * Usage:
 *   node docs/verification/store-purge.mjs --db PATH --session NAME [--session NAME …] [--dry-run]
 */

import { DatabaseSync } from "node:sqlite";
import { copyFileSync, existsSync } from "node:fs";

const argv = process.argv.slice(2);
const arg = (name, fallback) => {
  const i = argv.indexOf(`--${name}`);
  return i >= 0 && argv[i + 1] && !argv[i + 1].startsWith("--") ? argv[i + 1] : fallback;
};
const sessions = [];
for (let i = 0; i < argv.length; i += 1) {
  if (argv[i] === "--session" && argv[i + 1]) sessions.push(argv[i + 1]);
}
const DB = arg("db", null);
const DRY = argv.includes("--dry-run");

if (!DB) {
  console.error("store-purge: --db is required");
  process.exit(2);
}
if (sessions.length === 0) {
  console.error("store-purge: at least one --session is required, and there is no default");
  process.exit(2);
}
if (!existsSync(DB)) {
  console.error(`store-purge: no store at ${DB}`);
  process.exit(2);
}

// A copy first, always. Deleting from a memory store is not undoable and the cost of a backup is
// a file copy.
const backup = `${DB}.purge-backup`;
copyFileSync(DB, backup);
console.log(`backup  ${backup}`);

const db = new DatabaseSync(DB);

// Tables that key on a session, and the column that keys on it. `episodic_stream` is the root;
// edges and atlas rows hang off episode ids, so those are removed by subquery before the episodes
// themselves.
const plans = [
  ["dependency_graph_edge", "src_id IN (SELECT 'episode:' || episode_id FROM episodic_stream WHERE session_id = ?)"],
  ["semantic_atlas", "session_id = ?"],
  ["anchor_set", "session_id = ?"],
  ["symbolic_fact", "project_id = ?"],
  ["episodic_stream", "session_id = ?"],
];

console.log("");
for (const session of sessions) {
  let total = 0;
  for (const [table, where] of plans) {
    if (table === "symbolic_fact") continue; // project-scoped, not session-scoped
    let count = 0;
    try {
      count = db.prepare(`SELECT COUNT(*) AS n FROM ${table} WHERE ${where}`).get(session)?.n ?? 0;
    } catch {
      continue;
    }
    if (count === 0) continue;
    if (DRY) {
      total += count;
      console.log(`  would remove ${String(count).padStart(4)} from ${table.padEnd(22)} ${session}`);
      continue;
    }
    // # Report what happened, not what was attempted
    //
    // The first version printed "removed N from table" regardless of the outcome, and the
    // append-only trigger on `episodic_stream` rejects deletes — so it claimed to remove 60
    // episodes it had not touched. A cleanup tool that reports success without checking is worse
    // than no tool, because the next reader believes the store is clean.
    try {
      const changed = db.prepare(`DELETE FROM ${table} WHERE ${where}`).run(session).changes;
      total += changed;
      console.log(`  removed ${String(changed).padStart(4)} from ${table.padEnd(22)} ${session}`);
    } catch (error) {
      console.log(`  \x1b[33mkept\x1b[0m ${String(count).padStart(7)} in ${table.padEnd(22)} ${session}`);
      console.log(`         ${error.message.split("\n")[0].slice(0, 100)}`);
    }
  }
  if (total === 0) console.log(`  nothing matched ${session}`);
}

console.log("");
console.log(DRY ? "dry run: nothing was deleted" : "done");
db.close();
