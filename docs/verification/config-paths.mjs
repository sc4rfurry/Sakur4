#!/usr/bin/env node
/**
 * Check that every generated harness config names an absolute store path.
 *
 * `--db` defaults to the relative `sakur4.db`. Printing that into a harness's configuration is
 * a bug with a long fuse: the harness chooses the working directory when it spawns an MCP
 * server — Claude Desktop uses its own application folder — so the store lands somewhere the
 * user will never look, and a second harness quietly builds a second, empty memory.
 *
 * Run from an unrelated working directory, which is the condition that exposes it.
 */

import { spawnSync } from "node:child_process";
import { isAbsolute } from "node:path";

const bin =
  process.argv[2] ?? `${process.env.USERPROFILE ?? process.env.HOME}/.cargo/bin/sakur4d`;
const env = { ...process.env };
delete env.SAKUR4_DB;

const harnesses = ["hermes", "claude", "claude-code", "generic-http", "generic-stdio"];
let bad = 0;

for (const harness of harnesses) {
  const result = spawnSync(bin, ["config", harness], {
    encoding: "utf8",
    timeout: 60_000,
    env,
    // The whole point: a directory that has nothing to do with the project.
    cwd: process.env.TEMP ?? "/tmp",
  });
  const output = result.stdout ?? "";
  const match = output.match(/--db[ \t]+([^\s"]+)/);
  const store = match ? match[1] : null;

  if (harness === "claude") {
    // The JSON config nests the arguments, so a line match will not see it.
    const json = output.slice(output.indexOf("{"));
    let parsed = null;
    try {
      parsed = JSON.parse(json);
    } catch {
      /* reported below */
    }
    const args = parsed?.mcpServers?.sakur4?.args ?? [];
    const index = args.indexOf("--db");
    const nested = index >= 0 ? args[index + 1] : null;
    const ok = nested ? isAbsolute(nested) : false;
    if (!ok) bad += 1;
    console.log(
      `  ${harness.padEnd(15)} status=${result.status} db=${nested ?? "(none)"} ${ok ? "OK" : "NOT ABSOLUTE"}`,
    );
    continue;
  }

  if (harness === "hermes") {
    // YAML, and the arguments are a block list:
    //   args:
    //     - --db
    //     - "C:\\...\\sakur4.db"
    // A line-oriented match finds `- --db` and no value, which would report a false failure.
    const lines = output.split("\n").map((l) => l.trim());
    const index = lines.indexOf("- --db");
    const nested = index >= 0 ? (lines[index + 1] ?? "").replace(/^-\s*/, "").replace(/"/g, "") : null;
    const ok = nested ? isAbsolute(nested) : false;
    if (!ok) bad += 1;
    console.log(
      `  ${harness.padEnd(15)} status=${result.status} db=${nested ?? "(none)"} ${ok ? "OK" : "NOT ABSOLUTE"}`,
    );
    continue;
  }

  const ok = store ? isAbsolute(store) : false;
  if (!ok) bad += 1;
  console.log(
    `  ${harness.padEnd(15)} status=${result.status} db=${store ?? "(no --db flag)"} ${ok ? "OK" : "NOT ABSOLUTE"}`,
  );
}

console.log("");
if (bad === 0) {
  console.log("every harness config names an absolute store path");
  process.exit(0);
}
console.log(`${bad} harness config(s) name a relative store path`);
process.exit(1);
