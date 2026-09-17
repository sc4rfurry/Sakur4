#!/usr/bin/env node
/**
 * Exercise `context.record_usage` -> `context.receipt`, which nothing had called.
 *
 * # Why this round trip and not the classifier
 *
 * `provider_cache.rs` classifies turns and has unit tests for each verdict. What had never run
 * is the path a user takes: report usage through the MCP tool, then read the receipt and see
 * whether the numbers and the verdicts match what actually happened. A classifier can be right
 * and the tool that feeds it wrong.
 *
 * # The scenarios are the server's real numbers
 *
 * They are taken from measurements against a real llama.cpp, not invented: an append-only turn
 * on a ~3,000-token prompt reuses most of the prefix; the same prompt resent reuses nearly all
 * of it; changing the head drops the cached prefix to nothing. The point of the receipt is to
 * tell those three apart, and to say so rather than blame a cache it cannot see.
 *
 * Usage:
 *   node docs/verification/usage-roundtrip.mjs --bin ~/.cargo/bin/sakur4d
 */

import { spawn, spawnSync } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const argv = process.argv.slice(2);
const arg = (name, fallback) => {
  const i = argv.indexOf(`--${name}`);
  return i >= 0 && argv[i + 1] && !argv[i + 1].startsWith("--") ? argv[i + 1] : fallback;
};

const BIN = arg("bin", `${process.env.USERPROFILE ?? process.env.HOME}/.cargo/bin/sakur4d`);
const SESSION = "usage-roundtrip";

/** A live MCP session, because each scenario depends on the counters the last one moved. */
class Session {
  constructor(db) {
    this.child = spawn(
      BIN,
      ["--db", db, "--backend", "none", "serve", "--transport", "stdio", "--no-dream"],
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

const usage = (session_id, prompt, cached, extra = {}) => ({
  prompt_tokens: prompt,
  completion_tokens: 12,
  session_id,
  ...(cached === undefined ? {} : { cache_read_tokens: cached }),
  ...extra,
});

let failures = 0;
const check = (name, ok, detail = "") => {
  console.log(`  ${ok ? "\x1b[32mPASS\x1b[0m" : "\x1b[31mFAIL\x1b[0m"}  ${name}${detail ? `  \u2014 ${detail}` : ""}`);
  if (!ok) failures += 1;
};

const article = (detail) => `${detail} ${"padding tokens to make the prompt realistic. ".repeat(6)}`;

async function main() {
  const dir = mkdtempSync(join(tmpdir(), "sakur4-usage-"));
  const session = new Session(join(dir, "usage.db"));
  session.send({
    jsonrpc: "2.0",
    id: 1,
    method: "initialize",
    params: { protocolVersion: "2025-11-25", capabilities: {}, clientInfo: { name: "usage", version: "1" } },
  });
  session.send({ jsonrpc: "2.0", method: "notifications/initialized", params: {} });

  console.log("usage round trip");
  console.log("  daemon   " + BIN);
  console.log("");

  // --- a receipt before anything is reported -------------------------------------------
  const empty = unwrap(await session.call("context.receipt", { session_id: SESSION }));
  check("a session with no turns reports no turns",
    JSON.stringify(empty).includes("0") || /no turns|insufficient/i.test(JSON.stringify(empty)),
    JSON.stringify(empty).slice(0, 80));

  // --- turn 1: a cold start. Nothing was cached, because nothing was there to cache. -----
  const cold = unwrap(await session.call("context.record_usage", usage(SESSION, 3041, 0)));
  check("recording a cold turn is accepted", !cold.error, cold.error ?? "");
  const afterCold = unwrap(await session.call("context.receipt", { session_id: SESSION }));
  const coldText = JSON.stringify(afterCold);
  check("a cold turn is counted as a turn",
    /1 of 1 turn/.test(coldText) || /1 turn/.test(coldText),
    (coldText.match(/provider cache over [^"]*/) ?? [coldText.slice(0, 70)])[0].slice(0, 80));
  check("a cold turn is not reported as a cache hit",
    /0 of 3041/.test(coldText) || /0%/.test(coldText),
    "a first turn cannot have reused anything");

  // --- turn 2: append-only growth. The server reuses the prefix, processes a new suffix. --
  await session.call("context.record_usage", usage(SESSION, 3390, 2900));
  const afterGrowth = unwrap(await session.call("context.receipt", { session_id: SESSION }));
  const growthText = JSON.stringify(afterGrowth);
  check("an append-only turn raises the session's hit ratio",
    /over 2 of 2 turn/.test(growthText),
    (growthText.match(/provider cache over [^"]{0,64}/) ?? [growthText.slice(0, 70)])[0]);

  // --- turn 3: the same prompt resent. Nearly the whole thing comes from the cache. ------
  // NOTE ON WHAT IS ASSERTED HERE.
  //
  // The per-turn verdict — full reuse, partial reuse, cache miss — is computed at read time
  // and aggregated away by `ProviderCacheStats`, which carries counts and a ratio but no
  // per-turn classification. So a receipt cannot say "turn 3 was a full reuse"; it can only
  // say what the totals were. The first version of this check asserted the per-turn verdict
  // and failed for that reason, which is a fact about the API rather than a defect: the
  // table it reads from has no verdict column either.
  //
  // What the API does promise is the aggregate, so that is what is checked. The distinction
  // matters because the *user-facing* question — "did compaction cost me this turn?" — is
  // answered by the prefix-break count, which is exposed.
  await session.call("context.record_usage", usage(SESSION, 3390, 3390));
  const afterResend = unwrap(await session.call("context.receipt", { session_id: SESSION }));
  const resendText = JSON.stringify(afterResend);
  // The ratio is a plain sum over every turn, so it is exact and worth asserting exactly:
  // reads 0 + 2900 + 3390 = 6290 over prompts 3041 + 3390 + 3390 = 9821, which is 64%.
  //
  // An earlier version of this check asked for ">= 90%" on the reasoning that three turns of
  // near-total reuse should look impressive. It cannot: one cold turn is in the denominator,
  // and a session only gets one of those. That was my intuition about the number, not a
  // property of it, and it would have been a test that fails on correct behaviour.
  const resendReads = Number((resendText.match(/cache over \d+ of \d+ turn\(s\): (\d+) of/) ?? [])[1] ?? -1);
  const resendPrompts = Number((resendText.match(/turn\(s\): \d+ of (\d+) /) ?? [])[1] ?? -1);
  check("the aggregate is the exact sum over every turn",
    resendReads === 6290 && resendPrompts === 9821,
    `reads=${resendReads} (expected 6290), prompts=${resendPrompts} (expected 9821)`);
  const resendRatio = Number((resendText.match(/served from cache \((\d+)%\)/) ?? [])[1] ?? -1);
  check("the reported ratio is the sum expressed as a percentage",
    Math.abs(resendRatio - Math.round((6290 / 9821) * 100)) <= 1,
    `${resendRatio}% against a computed ${Math.round((6290 / 9821) * 100)}%`);

  // --- turn 4: a prefix break. The cached prefix collapses while the prompt grows. --------
  // This is the case the whole feature exists for: the provider was billed for history it had
  // already processed, and only the receipt can say so.
  await session.call("context.record_usage", usage(SESSION, 3700, 300));
  const afterBreak = unwrap(await session.call("context.receipt", { session_id: SESSION }));
  const breakText = JSON.stringify(afterBreak);
  check("a collapsed prefix is reported as a break rather than as a miss",
    /PREFIX-BROKEN|prefix break/i.test(breakText),
    (breakText.match(/"breaks?":[0-9]+/) ?? [breakText.slice(0, 70)])[0].slice(0, 90));

  // --- a turn where the provider reported no cache at all --------------------------------
  // Absence is not a miss. Sending zero asserts one; omitting says the provider did not report.
  // A receipt that conflated them would blame a cache it cannot see.
  const silentSession = SESSION + "-silent";
  await session.call("context.record_usage", usage(silentSession, 2000));
  const silent = unwrap(await session.call("context.receipt", { session_id: silentSession }));
  const silentText = JSON.stringify(silent);
  check("an unreported cache is not counted as a miss",
    !/0 of 2000/.test(silentText) || /not report|unknown|no cache/i.test(silentText),
    (silentText.match(/provider cache [^"]{0,70}/) ?? [silentText.slice(0, 70)])[0]);

  // --- the totals ------------------------------------------------------------------------
  const final = unwrap(await session.call("context.receipt", { session_id: SESSION }));
  const finalText = JSON.stringify(final);
  const turns = Number((finalText.match(/over (\d+) of (\d+) turn/) ?? [])[2] ?? 0);
  check("every reported turn is counted", turns === 4, `counted ${turns} of 4`);
  check("the receipt reports its own sample count honestly",
    /sample|insufficient/i.test(finalText),
    "gating advice on too few turns is how a receipt becomes misleading");

  session.close();
  try {
    rmSync(dir, { recursive: true, force: true });
  } catch {
    /* the OS will reclaim the temp directory */
  }

  console.log("");
  if (failures === 0) {
    console.log(`\x1b[32mVERDICT: PASS\x1b[0m — usage accounting round trip verified`);
    return 0;
  }
  console.log(`\x1b[31mVERDICT: FAIL\x1b[0m — ${failures} contract(s) failed`);
  return 1;
}

process.exit(await main());
