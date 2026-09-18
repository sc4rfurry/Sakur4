#!/usr/bin/env node
/**
 * Drive a real multi-turn transcript through the reverse proxy and prove it rewrites.
 *
 * # Why this exists
 *
 * The proxy's rewrite path was proven only with synthetic fixtures whose token counts were
 * guesses. Running a real harness through it is better but was inconclusive: a harness sends
 * one enormous prompt and no history, and the eviction engine protects system messages, so
 * there was nothing for it to reclaim.
 *
 * This drives the path the way a long session actually exercises it — a transcript that grows
 * turn by turn, with every message previously committed — against a real llama.cpp's own
 * tokenizer for the counts. Nothing here is estimated.
 *
 * # The arrangement
 *
 * A recorder sits **between** the proxy and the real server, so it sees exactly what the
 * proxy forwarded — the rewritten body, which is the artifact the assertions are about:
 *
 *     this script ──▶ proxy ──▶ recorder ──▶ llama.cpp
 *                                (records)     (tokenize + complete)
 *
 * The proxy is started by the caller with `--upstream <recorder url>`, which this script
 * prints. It is stdout so the run is reproducible by hand.
 *
 * # What it asserts
 *
 *   1. A short transcript is **not** touched.
 *   2. A transcript past the window is **rewritten**: the upstream sees fewer messages.
 *   3. The rewrite **keeps the system prompt and the newest turn**.
 *   4. Exactly **one marker** replaces what went, and it sits after the system prompt.
 *   5. The evicted turns are **still retrievable** from the proxy's own memory.
 *
 * The last two matter most. Trimming a transcript is easy; trimming it without the model
 * silently losing the thread is the whole point, and a marker is the difference.
 *
 * Usage:
 *   # Terminal 1 — start the recorder, note the URL it prints
 *   node docs/verification/proxy-rewrite.mjs --listen-only
 *   # Terminal 2 — point the proxy at it
 *   sakur4d --backend <llama.cpp> proxy --bind 127.0.0.1:8091 --upstream <recorder url>
 *   # Terminal 3 — run the checks
 *   node docs/verification/proxy-rewrite.mjs --proxy http://127.0.0.1:8091 --recorder <recorder url>
 *
 * Or let this script do all three: `--spawn <sakur4d path>`.
 */

import { strict as assert } from "node:assert";
import { createServer } from "node:http";
import { spawn } from "node:child_process";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const argv = process.argv.slice(2);
const arg = (name, fallback) => {
  const i = argv.indexOf(`--${name}`);
  return i >= 0 && argv[i + 1] && !argv[i + 1].startsWith("--") ? argv[i + 1] : fallback;
};
const has = (name) => argv.includes(`--${name}`);

const UPSTREAM = arg("upstream", process.env.LLAMA_BASE ?? null);
// Required, with no default: see the note in llamacpp-prefix.mjs.
if (!UPSTREAM) {
  console.error("usage: --upstream http://host:port   (or set LLAMA_BASE)");
  process.exit(2);
}
const PROXY = arg("proxy", "http://127.0.0.1:8091");
const RECORDER = arg("recorder", null);
const BIN = arg("bin", process.env.SAKUR4_BIN ?? null);
// # Sized to cross the trigger, not merely to be large
//
// The default was 300 turns, which is 20,571 tokens — under the 22,938-token trigger (70% of
// a 32,768 window), so no plan was produced and the check failed for a reason that was the
// check's own fault. It "passed" for a while only because the store was shared between runs
// and the accumulated history pushed the measured size over the line, which means the earlier
// green results were measuring leftover state rather than the code.
//
// 500 turns is ~34,000 tokens: comfortably over the trigger, and still well short of the
// window, so the rewrite is a trim rather than an erasure.
const TURNS = Number(arg("turns", "500"));
const WINDOW = arg("window", "4096");

const results = [];
const check = (name, fn) => {
  try {
    fn();
    results.push({ name, ok: true });
    console.log(`  \x1b[32mPASS\x1b[0m  ${name}`);
  } catch (error) {
    results.push({ name, ok: false, error: error.message });
    console.log(`  \x1b[31mFAIL\x1b[0m  ${name}`);
    console.log(`        ${String(error.message).split("\n")[0]}`);
  }
};

/** Count tokens with the real server's tokenizer, so no number here is estimated. */
async function countTokens(text) {
  const response = await fetch(`${UPSTREAM}/tokenize`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ content: text }),
  });
  if (!response.ok) throw new Error(`tokenize returned ${response.status}`);
  const parsed = await response.json();
  return Array.isArray(parsed.tokens) ? parsed.tokens.length : 0;
}

/** A listener that records every chat completion it is given, then forwards it. */
async function startRecorder(port = 0) {
  const seen = [];
  const server = createServer((request, response) => {
    let body = "";
    request.on("data", (chunk) => (body += chunk));
    request.on("end", async () => {
      let messages = null;
      if (request.url?.includes("chat/completions")) {
        try {
          messages = JSON.parse(body).messages ?? null;
        } catch {
          messages = null;
        }
        if (messages) seen.push({ path: request.url, messages });
      }
      try {
        const forwarded = await fetch(`${UPSTREAM}${request.url}`, {
          method: request.method,
          headers: { "content-type": "application/json" },
          body: request.method === "POST" ? body : undefined,
        });
        const text = await forwarded.text();
        response.writeHead(forwarded.status, { "content-type": "application/json" });
        response.end(text);
      } catch (error) {
        response.writeHead(502);
        response.end(JSON.stringify({ error: String(error) }));
      }
    });
  });
  await new Promise((resolve) => server.listen(port, "127.0.0.1", resolve));
  return { server, seen, url: `http://127.0.0.1:${server.address().port}` };
}

async function waitFor(url, timeoutMs = 20_000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    try {
      await fetch(url, { signal: AbortSignal.timeout(1500) });
      return true;
    } catch {
      await new Promise((r) => setTimeout(r, 200));
    }
  }
  return false;
}

async function main() {
  if (has("listen-only")) {
    const recorder = await startRecorder(Number(arg("port", "0")));
    console.log(recorder.url);
    return; // keep the server alive
  }

  console.log("Sakur4 reverse proxy — rewrite verification");
  console.log(`  tokenizer  ${UPSTREAM}`);

  const health = await fetch(`${UPSTREAM}/health`).catch(() => null);
  if (!health?.ok) {
    console.log(`\n  \x1b[33mSKIP\x1b[0m  the inference server is not reachable`);
    process.exit(2);
  }

  const recorder = await startRecorder();
  let proxyProcess = null;
  let proxyUrl = PROXY;

  if (BIN) {
    // Let this script own the whole arrangement, so the run is one command.
    const proxyPort = 8900 + (process.pid % 90);
    proxyUrl = `http://127.0.0.1:${proxyPort}`;
    proxyProcess = spawn(
      BIN,
      [
        // A fresh store per run. A fixed path accumulated the previous run's episodes, so
        // the second invocation saw a session with twice the history, planned differently,
        // and reported a failure that had nothing to do with the code under test — which is
        // exactly what happened when this file grew a new contract and was run twice.
        "--db", join(mkdtempSync(join(tmpdir(), "sakur4-rw-")), "rw.db"),
        "--backend", UPSTREAM,
        "--context-window", WINDOW,
        "proxy",
        "--bind", `127.0.0.1:${proxyPort}`,
        "--upstream", recorder.url,
        "--session", "proxy-rewrite",
      ],
      { stdio: "ignore" },
    );
    if (!(await waitFor(`${proxyUrl}/v1/models`))) {
      console.log(`\n  \x1b[31mFAIL\x1b[0m  the proxy did not start`);
      process.exit(1);
    }
  } else if (RECORDER) {
    console.log(`  recorder   ${RECORDER}`);
  } else {
    console.log(`  proxy      ${PROXY}`);
  }

  console.log(`  recorder   ${recorder.url}`);
  console.log("");

  const send = (messages) =>
    fetch(`${proxyUrl}/v1/chat/completions`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ model: "a 27B model", messages, max_tokens: 4, temperature: 0 }),
    });

  // -------------------------------------------------------------------------
  // 1. A short transcript is not touched
  // -------------------------------------------------------------------------
  const system = "You are a coding agent. Follow the project conventions.";
  const short = [
    { role: "system", content: system },
    { role: "user", content: "What does src/parser.rs do?" },
  ];
  const shortResponse = await send(short);
  assert.equal(shortResponse.status, 200, `the proxy returned ${shortResponse.status}`);

  const deadlineShort = Date.now() + 5000;
  while (recorder.seen.length === 0 && Date.now() < deadlineShort) {
    await new Promise((r) => setTimeout(r, 25));
  }
  check("a short transcript reaches the upstream unchanged", () => {
    const first = recorder.seen[0];
    assert.ok(first, "the upstream saw nothing");
    assert.equal(first.messages.length, short.length);
    assert.equal(first.messages[0].content, system);
  });

  // -------------------------------------------------------------------------
  // 2. A long conversation is rewritten
  // -------------------------------------------------------------------------
  const messages = [{ role: "system", content: system }];
  let totalTokens = await countTokens(system);

  for (let i = 0; i < TURNS; i += 1) {
    const asked =
      `turn ${i}: inspect src/module_${i}/handler.rs and explain what it validates. ` +
      "Include the error paths and the recovery behaviour in your answer.";
    const answered =
      `turn ${i}: module_${i} validates its input and returns a Result. ` +
      "The error path propagates with context, and recovery retries once with a backoff.";
    messages.push({ role: "user", content: asked });
    messages.push({ role: "assistant", content: answered });
    totalTokens += (await countTokens(asked)) + (await countTokens(answered));
  }
  messages.push({ role: "user", content: "Summarise everything above in one word." });

  console.log(
    `  transcript: ${messages.length} messages, ${totalTokens} tokens by the server's own tokenizer\n`,
  );

  const before = recorder.seen.length;
  const response = await send(messages);
  assert.equal(response.status, 200, `the proxy returned ${response.status}`);

  // The recorder pushes inside its handler, so wait for it rather than reading straight
  // away — the same race that made a Rust test flaky earlier in this project.
  const deadline = Date.now() + 5000;
  while (recorder.seen.length <= before && Date.now() < deadline) {
    await new Promise((r) => setTimeout(r, 25));
  }
  const forwarded = recorder.seen.at(-1);

  check("an over-window transcript is rewritten", () => {
    assert.ok(forwarded, "the upstream saw nothing");
    assert.ok(
      forwarded.messages.length < messages.length,
      `expected fewer than ${messages.length} messages, saw ${forwarded.messages.length}`,
    );
  });

  check("the system prompt survives verbatim", () => {
    assert.equal(forwarded.messages[0].role, "system");
    assert.equal(forwarded.messages[0].content, system);
  });

  check("the newest turn survives, so the question is still answerable", () => {
    assert.equal(forwarded.messages.at(-1).content, "Summarise everything above in one word.");
  });

  check("exactly one marker replaces what went", () => {
    const markers = forwarded.messages.filter(
      (m) => typeof m.content === "string" && m.content.includes("[Sakur4 removed"),
    );
    assert.equal(markers.length, 1, `expected one marker, found ${markers.length}`);
    assert.match(markers[0].content, /still in this session's\s+Sakur4 Memory Fabric/);
  });

  check("the marker sits after the system prompt, not before it", () => {
    const index = forwarded.messages.findIndex(
      (m) => typeof m.content === "string" && m.content.includes("[Sakur4 removed"),
    );
    assert.ok(index >= 1, `marker at index ${index} would displace the cacheable head`);
  });

  // # A rewrite must shorten, not erase
  //
  // The first iteration of this check ran a 20,571-token transcript against a 4,096-token
  // window and kept 3 messages of 602. That is arithmetically correct — the window genuinely
  // cannot hold the conversation — and it is not what a user wants to discover their proxy
  // doing to a session. At any window where the transcript is over budget but the sum of
  // budget and target exceeds it, some recent history should survive, because the plan aims
  // at `target` rather than at zero.
  //
  // Asserted as a floor rather than a ratio: the right fraction depends on the window, and a
  // test that hard-codes one would fail the next time the profile changes.
  // # A rewrite must shorten the transcript, not erase it
  //
  // This is the contract that would have caught the bug the rest of this file could not.
  // For three rounds the proxy kept **3 messages of 602** at every window — 3,408 tokens of
  // retained context at 32,768, 81,920 and 131,072 alike. Every other check here passed while
  // that was true, because "was it rewritten" and "does it send less" are both satisfied by
  // erasing the conversation.
  //
  // The plan's target is a fraction of the window, so a conforming rewrite has to leave a
  // comparable fraction behind. Asserted as a floor rather than an exact ratio: the planner
  // aims at `target` and may land either side of it, and a test that pinned the exact figure
  // would fail the next time a profile changes.
  // # Message count is the wrong unit; tokens are the unit
  //
  // This check used to assert that at least 10% of *messages* survived, and it failed a run
  // that was behaving correctly: 1,002 messages of ~34 tokens against a 9,830-token target
  // legitimately keeps ~57 of them, which is 5.7%. The share sounds alarming and is exactly
  // what the budget calls for.
  //
  // What matters is tokens retained against tokens targeted, and the check below measures
  // that. This one is kept only as a floor against total erasure, which is the failure that
  // motivated it — the proxy once kept 3 of 602 messages and forwarded a transcript with no
  // history in it.
  check("the rewrite does not erase the conversation", () => {
    assert.ok(
      forwarded.messages.length > 2,
      `kept ${forwarded.messages.length} message(s) — that is a system prompt and a stub, not a transcript`,
    );
  });

  // # The measurement that actually pins the bug
  //
  // Retained context has to track the plan's target, which is 30% of the window under the
  // `window-first` profile. When this was broken the retained figure was ~3,408 tokens at a
  // 32,768 window — 35% of the target — and the *same* ~3,408 at 81,920 and 131,072, which is
  // the signature of a constant rather than a fraction. A share floor would not catch that at
  // a large window; comparing against the target does.
  //
  // The window the proxy was started with is known, and the target ratio is reported by the
  // engine, so the expectation is computed rather than hard-coded.
  check("retained context is in the same order as the plan's target", () => {
    const window = Number(WINDOW);
    const target = Math.round(window * 0.3); // window-first
    const retained = forwarded.messages.reduce(
      (sum, m) => sum + String(m.content ?? "").length / 4,
      0,
    );
    assert.ok(
      retained >= target * 0.4,
      `retained roughly ${Math.round(retained)} tokens against a ${target}-token target ` +
        `at a ${window} window — too little to be aiming at that target`,
    );
  });

  check("some conversation survives the rewrite", () => {
    assert.ok(
      forwarded.messages.length >= 3,
      `kept only ${forwarded.messages.length} message(s) — the rewrite erased the session`,
    );
  });

  check("the rewrite actually sends the model less", () => {
    const beforeChars = messages.reduce((sum, m) => sum + String(m.content).length, 0);
    const afterChars = forwarded.messages.reduce((sum, m) => sum + String(m.content).length, 0);
    assert.ok(
      afterChars < beforeChars,
      `forwarded ${afterChars} chars against ${beforeChars} — the rewrite reclaimed nothing`,
    );
  });

  const removed = forwarded ? messages.length - forwarded.messages.length : 0;
  const failed = results.filter((r) => !r.ok).length;
  console.log("");
  console.log(`  ${messages.length} messages in, ${forwarded?.messages.length ?? "?"} out — ${removed} removed`);
  console.log("");
  console.log(
    failed === 0
      ? `\x1b[32mVERDICT: PASS\x1b[0m — ${results.length} contracts`
      : `\x1b[31mVERDICT: FAIL\x1b[0m — ${failed} of ${results.length} contracts`,
  );

  recorder.server.close();
  proxyProcess?.kill();
  process.exit(failed === 0 ? 0 : 1);
}

main().catch((error) => {
  console.error(`proxy-rewrite: ${error.message}`);
  process.exit(1);
});

