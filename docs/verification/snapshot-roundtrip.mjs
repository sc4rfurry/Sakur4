#!/usr/bin/env node
/**
 * Exercise the snapshot / restore pair end to end.
 *
 * # What this is for
 *
 * `session.snapshot` persists a slot's KV cache so a later turn can skip the prefill;
 * `session.restore` reads it back. FR-8 calls this the mechanism that makes a long session
 * survivable across days. Neither had ever been called.
 *
 * # The two paths, and why both matter
 *
 * The user's llama.cpp build advertises `slots+tokenize` and returns **501** for
 * `?action=save`. So the interesting question here is not only "does the happy path work"
 * but "what does a user on a server without the API actually see". A tool that throws an
 * unhandled error, or worse reports success without saving anything, is the failure worth
 * catching — the happy path is the easy half.
 *
 * Usage:
 *   node docs/verification/snapshot-roundtrip.mjs --bin ~/.cargo/bin/sakur4d --backend embedded
 *   node docs/verification/snapshot-roundtrip.mjs --bin ~/.cargo/bin/sakur4d --backend http://host:8080
 */

import { spawn, spawnSync } from "node:child_process";

const argv = process.argv.slice(2);
const arg = (name, fallback) => {
  const i = argv.indexOf(`--${name}`);
  return i >= 0 && argv[i + 1] && !argv[i + 1].startsWith("--") ? argv[i + 1] : fallback;
};

const BIN = arg("bin", `${process.env.USERPROFILE ?? process.env.HOME}/.cargo/bin/sakur4d`);
const BACKEND = arg("backend", "embedded");
const DB = arg("db", ":memory:");
const SESSION = "snapshot-check";

/**
 * A live MCP session over stdio, so a later call can use a value an earlier one produced.
 *
 * The first version of this script batched its calls into one `spawnSync` input, which cannot
 * do that — and the round trip needs it, because the restore path is only known after the
 * snapshot returns. Batching was fine while the test only checked failures.
 */
class Session {
  constructor() {
    this.child = spawn(
      BIN,
      ["--db", DB, "--backend", BACKEND, "serve", "--transport", "stdio", "--no-dream"],
      { stdio: ["pipe", "pipe", "pipe"] },
    );
    this.buffer = "";
    this.pending = new Map();
    this.nextId = 10;
    this.child.stdout.on("data", (chunk) => {
      this.buffer += chunk.toString();
      let index;
      while ((index = this.buffer.indexOf("\n")) >= 0) {
        const line = this.buffer.slice(0, index).trim();
        this.buffer = this.buffer.slice(index + 1);
        if (!line.startsWith("{")) continue;
        let parsed;
        try {
          parsed = JSON.parse(line);
        } catch {
          continue;
        }
        const resolve = this.pending.get(parsed.id);
        if (!resolve) continue;
        this.pending.delete(parsed.id);
        resolve(parsed);
      }
    });
  }

  send(frame) {
    this.child.stdin.write(JSON.stringify(frame) + "\n");
  }

  call(name, args) {
    const id = this.nextId++;
    return new Promise((resolve) => {
      this.pending.set(id, resolve);
      this.send({ jsonrpc: "2.0", id, method: "tools/call", params: { name, arguments: args } });
      setTimeout(() => {
        if (this.pending.delete(id)) resolve({ error: { message: "timed out" } });
      }, 60_000);
    });
  }

  close() {
    try {
      this.child.kill();
    } catch {
      /* already gone */
    }
  }
}

function unwrap(parsed) {
  if (!parsed) return {};
  if (parsed.error) return { error: parsed.error.message ?? String(parsed.error) };
  const sc = parsed.result?.structuredContent;
  if (sc) return sc;
  try {
    return JSON.parse(parsed.result?.content?.[0]?.text ?? "null") ?? {};
  } catch {
    return { text: parsed.result?.content?.[0]?.text ?? "" };
  }
}

let failures = 0;
const check = (name, ok, detail = "") => {
  console.log(`  ${ok ? "\x1b[32mPASS\x1b[0m" : "\x1b[31mFAIL\x1b[0m"}  ${name}${detail ? `  \u2014 ${detail}` : ""}`);
  if (!ok) failures += 1;
};

console.log(`snapshot round trip`);
console.log(`  daemon   ${BIN}`);
console.log(`  backend  ${BACKEND}`);
console.log("");

const cached = BACKEND === "embedded";
console.log(
  `  (this backend ${cached ? "advertises" : "may not advertise"} kv save/restore; ` +
    `${cached ? "the happy path" : "degradation is what is under test"})\n`,
);

const session = new Session();
session.send({
  jsonrpc: "2.0",
  id: 1,
  method: "initialize",
  params: { protocolVersion: "2025-11-25", capabilities: {}, clientInfo: { name: "snap", version: "1" } },
});
session.send({ jsonrpc: "2.0", method: "notifications/initialized", params: {} });

const status = unwrap(await session.call("sakur4.status", {}));
check("the daemon reports its capabilities", typeof status.capabilities === "string", status.capabilities ?? "");
const supportsSave = /save/.test(status.capabilities ?? "");

const snapshot = unwrap(await session.call("session.snapshot", { session_id: SESSION, slot_id: "0" }));
const snapOk = !snapshot.error && typeof snapshot.snapshot_id === "string";

if (supportsSave) {
  check("snapshot returns an id and a path when the backend supports it", snapOk,
    snapshot.error ?? `${snapshot.snapshot_id ?? "?"} -> ${snapshot.file_path ?? "?"}`);

  // The half that was missing at first: restoring the file the snapshot just wrote. A round
  // trip that only tries a path which does not exist proves the error branch and nothing else.
  const back = unwrap(await session.call("session.restore", {
    session_id: SESSION,
    slot_id: "0",
    path: snapshot.file_path,
  }));
  check("restore accepts the snapshot the daemon just wrote",
    !back.error,
    back.error ?? "restored");
} else {
  // On a backend without the API, the question is whether the refusal is clear. A tool that
  // silently reported success without saving would leave a user believing their session is
  // recoverable when it is not, which is worse than an error.
  check("snapshot refuses clearly when the backend cannot save",
    !snapOk && typeof snapshot.error === "string",
    snapshot.error ?? "reported success without saving anything");
  check("the refusal names the reason rather than failing generically",
    /save|restore|snapshot|checkpoint|not implemented|501|support/i.test(snapshot.error ?? ""),
    (snapshot.error ?? "").slice(0, 90));
}

const restore = unwrap(await session.call("session.restore", {
  session_id: SESSION, slot_id: "0", path: "/nonexistent/path.bin",
}));
const missing = restore.error ?? "";
check("restore of a path that does not exist fails rather than reporting success",
  typeof missing === "string" && missing.length > 0,
  missing.slice(0, 90));
check("the missing-file failure is legible",
  /no such file|not found|missing|cannot|unable|no file|404|not implemented/i.test(missing),
  missing.slice(0, 90) || "(no error reported — a restore that cannot find its file must say so)");

session.close();
console.log("");
if (failures === 0) {
  console.log(`\x1b[32mVERDICT: PASS\x1b[0m — ${cached ? "happy path" : "degradation"} verified`);
  process.exit(0);
}
console.log(`\x1b[31mVERDICT: FAIL\x1b[0m — ${failures} contract(s) failed`);
process.exit(1);
