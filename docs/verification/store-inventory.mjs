#!/usr/bin/env node
/**
 * Show what is in a Sakur4 store, grouped by session.
 *
 * Used to find the test data this repository's verification runs left in the *default* store.
 * Several checks invoke the skill or the daemon without `SAKUR4_DB` set, which resolves to
 * `~/.sakur4/sakur4.db` — the same store a person's real sessions use. That is the right
 * default for a user and the wrong one for a test, and the difference only shows up here.
 *
 * Usage:
 *   node docs/verification/store-inventory.mjs [--db PATH]
 */

import { DatabaseSync } from "node:sqlite";
import { existsSync } from "node:fs";
import { join } from "node:path";

const argv = process.argv.slice(2);
const arg = (name, fallback) => {
  const i = argv.indexOf(`--${name}`);
  return i >= 0 && argv[i + 1] && !argv[i + 1].startsWith("--") ? argv[i + 1] : fallback;
};

const DB = arg("db", join(process.env.USERPROFILE ?? process.env.HOME, ".sakur4", "sakur4.db"));

if (!existsSync(DB)) {
  console.log(`no store at ${DB}`);
  process.exit(0);
}

const db = new DatabaseSync(DB);
const scalar = (sql) => {
  try {
    return db.prepare(sql).get()?.n ?? 0;
  } catch {
    return 0;
  }
};

console.log(`store  ${DB}`);
console.log(`  episodes  ${scalar("SELECT COUNT(*) AS n FROM episodic_stream")}`);
console.log(`  anchors   ${scalar("SELECT COUNT(*) AS n FROM anchor_set")}`);
console.log("");

const rows = db
  .prepare(
    `SELECT session_id,
            COUNT(*)                                   AS episodes,
            SUM(CASE WHEN eviction_tier <> 'live' THEN 1 ELSE 0 END) AS moved,
            MIN(created_at)                            AS first_seen
       FROM episodic_stream
      GROUP BY session_id
      ORDER BY episodes DESC`,
  )
  .all();

if (rows.length === 0) {
  console.log("  (no episodes)");
} else {
  console.log("  episodes  session");
  for (const r of rows) {
    console.log(`  ${String(r.episodes).padStart(8)}  ${r.session_id}`);
  }
}

console.log("");
const anchors = db.prepare("SELECT session_id, COUNT(*) AS n FROM anchor_set GROUP BY session_id").all();
if (anchors.length > 0) {
  console.log("  anchors   session");
  for (const a of anchors) {
    console.log(`  ${String(a.n).padStart(8)}  ${a.session_id}`);
  }
}

db.close();
