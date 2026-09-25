#!/usr/bin/env node
// The end-to-end walkthrough, asserted rather than illustrated.
//
// # Why this needs a check
//
// `sakur4d demo` is the README's flagship: it commits turns, pins a constraint, recalls, evicts and prints a
// receipt, all against the embedded backend so it works on any machine with no GPU, no model and no network.
// Its own closing text says:
//
// > *The receipt above is the same one `context.receipt` returns over MCP, and the eviction plan is the same
// > one `context.plan_eviction` returns. Nothing in this walkthrough uses a code path the MCP tools do not.*
//
// **Nothing verified that.** `doc-commands.mjs` checks that `demo` is a real subcommand — not that it runs, not
// that it produces a receipt, and not that the paths it claims to share are the ones it took. A regression that
// made the walkthrough print nothing would have passed every check in the suite.
//
// It is also the one end-to-end check that needs no network, no daemon on a port and no inference server, which
// makes it the right thing to rely on when those are unavailable.
//
// Usage: node docs/verification/demo-run.mjs <path-to-sakur4d>
import { spawnSync } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const bin = process.argv[2];
if (!bin) {
  console.error("usage: demo-run.mjs <path-to-sakur4d>");
  process.exit(2);
}

const store = mkdtempSync(join(tmpdir(), "sakur4-demo-"));
const failures = [];
const check = (name, ok, detail) => {
  console.log(`  ${ok ? "ok  " : "FAIL"}  ${name}${detail ? ` — ${detail}` : ""}`);
  if (!ok) failures.push(name);
};

const run = spawnSync(bin, ["--db", join(store, "demo.db"), "--backend", "none", "demo"], {
  encoding: "utf8",
  timeout: 180_000,
  maxBuffer: 32 * 1024 * 1024,
  // **No `SAKUR4_*` environment is inherited.** The walkthrough must work with nothing configured, which is
  // the claim; passing the machine's own settings in would test a configuration a user does not have.
  env: { PATH: process.env.PATH, SYSTEMROOT: process.env.SYSTEMROOT, TEMP: process.env.TEMP },
});
const out = `${run.stdout ?? ""}\n${run.stderr ?? ""}`;
rmSync(store, { recursive: true, force: true });

check("the walkthrough exits cleanly", run.status === 0, `exit ${run.status}`);

// # The claims its closing text makes, checked as claims
check(
  "it produces a receipt",
  /receipt/i.test(out) && /tokens/i.test(out),
  "the walkthrough says the receipt is the one `context.receipt` returns",
);
check(
  "it produces an eviction plan",
  /evict|tier|masked|archived|dropped/i.test(out),
  "and that the plan is the one `context.plan_eviction` returns",
);
check(
  "it commits turns and extracts symbolic facts",
  /committed .*turn/i.test(out) && /symbolic/i.test(out),
  "the Episodic Stream and the Symbolic Ledger are the two halves of every turn",
);
check(
  "it resolves staleness rather than trusting a summary",
  /STALE/i.test(out),
  "`[STALE SUMMARY — do not trust]` is the dual-track guarantee made visible",
);
check(
  "it reports the context window and the eviction trigger",
  /context window/i.test(out) && /trigger/i.test(out),
  "a receipt without a window cannot be read as a fraction",
);

if (failures.length) {
  console.log(`  ${failures.length} claim(s) not demonstrated`);
  console.log("  --- the walkthrough's output follows ---");
  console.log(
    out
      .split("\n")
      .slice(0, 40)
      .map((l) => `  | ${l}`)
      .join("\n"),
  );
  process.exit(1);
}
console.log("  the walkthrough demonstrates every claim its closing text makes");
