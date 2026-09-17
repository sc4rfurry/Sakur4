#!/usr/bin/env node
/**
 * Find semantic-atlas entries and report what anchors them.
 *
 * Written to answer one question: recall kept returning summaries of *test fixtures* after the
 * episodes behind them had been archived, so something else was holding them in the window. The
 * atlas is filtered by its own anchor, not by the episode's tier, which is the part that is easy
 * to get wrong — and was.
 *
 * Usage:
 *   node docs/verification/atlas-inventory.mjs [--db PATH] [--match TEXT]
 */

import { DatabaseSync } from "node:sqlite";
import { join } from "node:path";

const argv = process.argv.slice(2);
const arg = (name, fallback) => {
  const i = argv.indexOf(`--${name}`);
  return i >= 0 && argv[i + 1] && !argv[i + 1].startsWith("--") ? argv[i + 1] : fallback;
};

const DB = arg("db", join(process.env.USERPROFILE ?? process.env.HOME, ".sakur4", "sakur4.db"));
const MATCH = arg("match", null);

const db = new DatabaseSync(DB);

const where = MATCH ? "WHERE content LIKE ?" : "";
const params = MATCH ? [`%${MATCH}%`] : [];
const rows = db
  .prepare(
    `SELECT a.atlas_id,
            a.anchor_type,
            a.anchor_id,
            a.session_id,
            (SELECT e.session_id FROM episodic_stream e WHERE e.episode_id = a.anchor_id) AS episode_session,
            substr(a.content, 1, 60) AS preview
       FROM semantic_atlas a
       ${where}
      ORDER BY a.created_at`,
  )
  .all(...params);

console.log(`store  ${DB}`);
console.log(`atlas  ${rows.length} entr${rows.length === 1 ? "y" : "ies"}${MATCH ? ` matching '${MATCH}'` : ""}`);
console.log("");

if (rows.length === 0) process.exit(0);

console.log("  session_id          episode_session     anchor_type        preview");
for (const r of rows) {
  console.log(
    `  ${String(r.session_id ?? "(null)").padEnd(20)}` +
      `${String(r.episode_session ?? "(no episode)").padEnd(20)}` +
      `${String(r.anchor_type).padEnd(19)}${(r.preview ?? "").replace(/\s+/g, " ").slice(0, 40)}`,
  );
}

db.close();
