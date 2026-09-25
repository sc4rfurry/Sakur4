#!/usr/bin/env node
// Conformance against the **official** MCP TypeScript SDK.
//
// # The gap this closes
//
// Every documented claim about Sakur4's stdio transport rested on a **hand-rolled** JSON-RPC client in
// `crates/sakur4d/tests/stdio_transport.rs` — a client this project wrote, testing a server this project
// wrote. That is worth something and it is not a conformance test: if the server and the test share a
// misunderstanding of the protocol, both are happy.
//
// The HTTP tests use the real `rmcp` client, so one transport already had an independent implementation
// behind it. Stdio did not. **This drives the same daemon with the official SDK**, so a disagreement
// between Sakur4 and the reference implementation is now a failing check rather than something a reader
// has to take on trust.
//
// It is not the official MCP conformance *suite*, and `docs/DESIGN.md` still says so.
//
// Usage: node docs/verification/mcp-sdk-conformance.mjs <path-to-sakur4d>
import { spawnSync } from "node:child_process";
import { createRequire } from "node:module";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const bin = process.argv[2];
if (!bin) {
  console.error("usage: mcp-sdk-conformance.mjs <path-to-sakur4d>");
  process.exit(2);
}

// # The SDK is resolved rather than vendored
//
// Adding `@modelcontextprotocol/sdk` to this repository would mean a `node_modules` and a lockfile in a
// Rust project, for one check. `SAKUR4_MCP_SDK_PATH` points at an installation; without it the check
// reports that it cannot run rather than pretending to have passed.
const sdkPath = process.env.SAKUR4_MCP_SDK_PATH;
if (!sdkPath) {
  console.log("  SKIP: set SAKUR4_MCP_SDK_PATH to a directory with @modelcontextprotocol/sdk installed");
  console.log("        npm install --prefix <dir> @modelcontextprotocol/sdk");
  process.exit(0);
}

const require = createRequire(join(sdkPath, "package.json"));
let Client, StdioClientTransport;
try {
  ({ Client } = require("@modelcontextprotocol/sdk/client/index.js"));
  ({ StdioClientTransport } = require("@modelcontextprotocol/sdk/client/stdio.js"));
} catch (error) {
  console.log(`  SKIP: the SDK at ${sdkPath} could not be loaded (${error.message})`);
  process.exit(0);
}

const store = mkdtempSync(join(tmpdir(), "mcp-sdk-"));
const failures = [];
const check = (name, ok, detail) => {
  console.log(`  ${ok ? "ok  " : "FAIL"}  ${name}${detail ? ` — ${detail}` : ""}`);
  if (!ok) failures.push(name);
};

const transport = new StdioClientTransport({
  command: bin,
  args: ["--db", join(store, "sdk.db"), "--backend", "none", "serve", "--transport", "stdio", "--no-dream"],
  stderr: "pipe",
});
const client = new Client({ name: "sakur4-conformance", version: "0.0.0" }, { capabilities: {} });

try {
  // # 1. The SDK completes its own handshake
  //
  // Nothing is stubbed: the SDK decides what to send and how to interpret the answers, so a disagreement
  // about framing, `protocolVersion` or capabilities surfaces here.
  await client.connect(transport);
  const version = client.getServerVersion();
  check("the official SDK completes the handshake", Boolean(version?.name), `server ${version?.name}`);

  // # 2. The catalog is readable through a third-party client
  const tools = await client.listTools();
  const names = (tools.tools ?? []).map((t) => t.name);
  check("tools/list returns the whole surface", names.length === 17, `${names.length} tools`);
  check(
    "every tool carries an input schema",
    (tools.tools ?? []).every((t) => t.inputSchema && typeof t.inputSchema === "object"),
    "a tool without one is unusable to a generated client",
  );

  // # 3. A real call round-trips, with its structured content intact
  const commit = await client.callTool({
    name: "memory.commit_episode",
    arguments: { session_id: "sdk", role: "user", content: "the retry budget is three" },
  });
  const episode = commit.structuredContent?.episode_id;
  check("tools/call round-trips", Boolean(episode), `episode ${String(episode).slice(0, 24)}`);
  check("the result is not an error", commit.isError !== true, "isError must be absent or false");

  // # 4. The two calls in order, which is the fix this session shipped
  //
  // The SDK awaits each call, so this cannot exercise the batching defect — but it does confirm that a
  // third-party client sees a write it made, which is the property the defect broke.
  const status = await client.callTool({ name: "sakur4.status", arguments: {} });
  const episodes = status.structuredContent?.episodes;
  check("a read after a write sees it", Number(episodes) >= 1, `status reports ${episodes} episode(s)`);

  // # 5. Resources and prompts are advertised as the README claims
  const resources = await client.listResources();
  check("resources/list answers", (resources.resources ?? []).length === 4, `${(resources.resources ?? []).length} resources`);
  const prompts = await client.listPrompts();
  check("prompts/list answers", (prompts.prompts ?? []).length >= 1, `${(prompts.prompts ?? []).length} prompt(s)`);

  // # 6. An unknown tool is refused rather than ignored
  //
  // The SDK turns a JSON-RPC error into a throw, so reaching the next line means the server answered a
  // bad request with something the reference client considers valid.
  let refused = false;
  try {
    await client.callTool({ name: "does.not.exist", arguments: {} });
  } catch {
    refused = true;
  }
  check("an unknown tool is refused", refused, "a silent success would be worse than a failure");

  // # 7. The schemas' format keywords, recorded rather than asserted
  //
  // # What this found, and why it is a note and not a failure
  //
  // The official SDK prints **92 warnings** on `tools/list`: `unknown format "uint" ignored in schema at
  // path …`. `schemars` emits `"format": "uint"` for Rust's `usize`, and `uint` is not a JSON Schema
  // format — the standard names `int32`, `int64`, `float`, `double` and leaves the rest as annotations.
  //
  // **The spec is explicit that an unrecognised `format` must be ignored**, and the reference
  // implementation ignores it: every call in this file succeeded, including the ones whose arguments
  // contain `usize` fields. So this is not a conformance failure, and asserting it as one would be this
  // check being stricter than the standard it claims to test — which is the mistake the wiki link checker
  // and the workflow YAML heuristic both made.
  //
  // It is still worth reporting, because the two ways it bites are real and neither shows up here:
  // a client that *validates* strictly rather than ignoring the keyword would reject the schema, and 92
  // warnings per session is noise that teaches a reader to skip the log.
  //
  // **The fix is one substitution at the `schemars` layer** — `"uint"` to `"uint64"`, the standard name
  // for an unsigned 64-bit integer. It is deliberately not done here: it changes the schema for all 17
  // tools, which is a **wire-contract change** and belongs in a release with a changelog entry, not
  // bundled into the round that noticed it. `docs/DESIGN.md` records it with that reasoning.
  const formats = new Map();
  const walk = (node) => {
    if (!node || typeof node !== "object") return;
    if (typeof node.format === "string") formats.set(node.format, (formats.get(node.format) ?? 0) + 1);
    for (const value of Object.values(node)) walk(value);
  };
  for (const tool of tools.tools ?? []) walk(tool.inputSchema);
  const known = new Set(["date-time", "date", "time", "duration", "email", "uri", "uuid", "int32", "int64", "float", "double"]);
  const nonStandard = [...formats.keys()].filter((f) => !known.has(f));
  if (nonStandard.length) {
    console.log(
      `  note  ${nonStandard.length} non-standard schema format(s): ${nonStandard.join(", ")} — the spec says a client must ignore these, and this one does; see docs/DESIGN.md`,
    );
  } else {
    console.log(`  ok    every schema format is a standard one (${formats.size} in use)`);
  }

  await client.close();
} catch (error) {
  check("the session completed", false, error?.message ?? String(error));
}

rmSync(store, { recursive: true, force: true });

if (failures.length) {
  console.log(`  ${failures.length} check(s) failed against the official SDK`);
  process.exit(1);
}
console.log("  the official SDK and Sakur4 agree on the protocol");
