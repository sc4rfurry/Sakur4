#!/usr/bin/env node
/**
 * Run the exact commands the OMP extension invokes.
 *
 * The extension drives the daemon two ways: by spawning the CLI for some tools and by MCP tool
 * calls for others. Both were written against a documented interface, and neither had been
 * exercised independently of OMP — which means a renamed flag or a changed argument order would
 * only surface when a user ran the tool inside the harness.
 *
 * This runs the CLI forms with the exact argument shapes the extension builds, so a break shows
 * up here rather than in a session.
 */

import { spawnSync } from "node:child_process";

const bin =
  process.argv[2] ?? `${process.env.USERPROFILE ?? process.env.HOME}/.cargo/bin/sakur4d`;
const session = "omp-extension-check";

const cases = [
  // sakur4_commit: ["commit", session, content, "--role", role, ("--tool", toolName)]
  ["sakur4_commit user", ["commit", session, "add rate limiting to the login endpoint", "--role", "user"]],
  ["sakur4_commit assistant", ["commit", session, "I will add a token bucket limiter", "--role", "assistant"]],
  [
    "sakur4_commit tool",
    ["commit", session, '{"exit_code":0,"stdout":"ok"}', "--role", "tool", "--tool", "shell_exec"],
  ],
  // sakur4_pin: ["pin", content, "--kind", kind, "--session", session]
  [
    "sakur4_pin",
    ["pin", "keep the public API of src/auth.rs backward compatible", "--kind", "task_contract", "--session", session],
  ],
  // sakur4_recall: ["recall", query, "--k", k, "--session", session]
  ["sakur4_recall", ["recall", "how did we handle retries", "--k", "5", "--session", session]],
  // sakur4_receipt: ["receipt", session]
  ["sakur4_receipt", ["receipt", session]],
  // /sakur4 index: ["index", root]
  ["slash index", ["index", "."]],
  // /sakur4 map: ["repo-map", "--budget", budget]
  ["slash map", ["repo-map", "--budget", "2000"]],
  // /sakur4 dream, reported by the extension's shutdown notice
  ["slash dream", ["dream"]],
  ["staleness", ["staleness"]],
];

let failed = 0;
for (const [label, args] of cases) {
  const result = spawnSync(bin, args, {
    encoding: "utf8",
    timeout: 300_000,
    maxBuffer: 64 * 1024 * 1024,
  });
  const first = `${result.stdout ?? ""}${result.stderr ?? ""}`
    .split("\n")
    .filter((line) => line.trim())[0];
  const ok = result.status === 0;
  if (!ok) failed += 1;
  console.log(
    `  ${ok ? "ok  " : `EXIT${result.status}`} ${label.padEnd(24)} ${(first ?? "(no output)").trim().slice(0, 58)}`,
  );
}

console.log("");
if (failed === 0) {
  console.log(`all ${cases.length} extension command forms succeed`);
  process.exit(0);
}
console.log(`${failed} of ${cases.length} extension command forms failed`);
process.exit(1);
