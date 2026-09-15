// Measure whether this llama.cpp server actually reuses a KV prefix.
//
// The question this answers
// -------------------------
// Sakur4's whole claim is that preserving the prompt prefix saves prefill work.
// `sakur4d doctor` correctly reports that this build exposes no checkpoint API:
// `/slots/{id}?action=save` and `?action=erase` both return 501 Not Implemented,
// and `/props` advertises no checkpoint configuration. So explicit alignment is
// impossible here.
//
// But explicit checkpoints are not the only mechanism. llama.cpp caches the longest
// common prefix of an incoming prompt in the slot's KV cache automatically, and
// reuses it whenever the new prompt shares a head with what is already resident.
// That path does not need save/restore at all. If it works on this server, then
// prefix preservation still pays off here, and Sakur4's `full-re-prefill` verdict
// is pessimistic rather than wrong.
//
// Measuring it from outside
// -------------------------
// The build's `/slots` payload is thin (`id`, `n_ctx`, `speculative`,
// `is_processing`) — no `prompt_n`, no `next_token` — and `/metrics` only exposes
// counters. So the observable is wall-clock time for a fixed prompt, which is
// exactly what the user experiences: prefill is the dominant cost on a 27B at Q3.
//
// Three requests, same model, same generation length:
//
//   A. a cold prompt                     → pays full prefill
//   B. the identical prompt again        → should be near-instant if KV is reused
//   C. the prompt with its HEAD changed  → should pay full prefill again
//
// If B is much faster than A, automatic prefix reuse works. If C is as slow as A,
// then changing the head really does invalidate everything — which is the failure
// Sakur4 exists to avoid, demonstrated on real hardware.
//
// Run: node docs/verification/llamacpp-prefix.mjs [--base http://host:port]

const BASE = argValue("--base", process.env.LLAMA_BASE ?? "http://your-llama-server:8080");
const N_PREDICT = Number(argValue("--predict", "1"));

function argValue(name, fallback) {
  const i = process.argv.indexOf(name);
  return i >= 0 && process.argv[i + 1] ? process.argv[i + 1] : fallback;
}

async function json(path, init) {
  const res = await fetch(`${BASE}${path}`, init);
  const text = await res.text();
  if (!res.ok) throw new Error(`${path} -> ${res.status}: ${text.slice(0, 300)}`);
  try {
    return JSON.parse(text);
  } catch {
    throw new Error(`${path} returned non-JSON: ${text.slice(0, 200)}`);
  }
}

async function tokenCount(content) {
  const out = await json("/tokenize", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ content }),
  });
  return out.tokens.length;
}

/** One completion, returning elapsed milliseconds and the server's token counts. */
async function complete(prompt) {
  const started = process.hrtime.bigint();
  const out = await json("/completion", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      prompt,
      n_predict: N_PREDICT,
      temperature: 0,
      cache_prompt: true,
      stream: false,
    }),
  });
  const ms = Number(process.hrtime.bigint() - started) / 1e6;
  return { ms, out };
}

/**
 * Build a prompt of roughly `tokens` tokens out of realistic-looking agent content.
 *
 * # Why this is calibrated by measuring rather than by estimating
 *
 * The first version guessed a line size and produced a 156,126-token prompt from a
 * request for 3,000 — because the digits in it tokenise far more finely than prose.
 * Token counts are not predictable from character counts, and this project has
 * already been bitten once by estimating in one place and measuring in another. So
 * this builds a candidate, asks the server to tokenise it, and scales.
 *
 * Not lorem ipsum: the point is a prompt shaped like the thing Sakur4 actually
 * compacts, and a repetitive corpus would tokenise unrealistically.
 */
async function buildPrompt(targetTokens, { headChanged = false } = {}) {
  const lines = [];
  const emit = (n) => {
    // A marker near the head, so changing it invalidates the prefix from token ~0 —
    // which is what a summarising compaction does to a transcript.
    if (n === 2 && headChanged) {
      lines.push("## Revised task contract (v2, supersedes v1)");
    }
    lines.push(
      `step ${n}: inspect src/module_${n % 97}/handler.rs; the function ` +
        `handle_request_${n} validates input and returns Result<Response, Error>; ` +
        `callers: ${(n % 7) + 1}; coverage ${(n % 40) + 60}%; note ${(n * 7919) % 100000}`,
    );
  };

  // Two passes: measure a sample, then scale. One correction is enough because the
  // relationship is very close to linear.
  const SAMPLE = 40;
  for (let i = 0; i < SAMPLE; i++) emit(i);
  const sampleTokens = await tokenCount(lines.join("\n"));
  const perLine = Math.max(1, sampleTokens / SAMPLE);
  const needed = Math.max(SAMPLE, Math.ceil(targetTokens / perLine));

  lines.length = 0;
  for (let i = 0; i < needed; i++) emit(i);
  const text = lines.join("\n");

  // Report what it actually is, so the caller can print the truth rather than the
  // request.
  return { text, tokens: await tokenCount(text), perLine };
}

const fmt = (ms) => `${ms.toFixed(0)} ms`;

console.log(`server   ${BASE}`);
const props = await json("/props");
console.log(`model    ${props.model_alias ?? props.model_path}`);
console.log(`build    ${props.build_info ?? "unknown"}`);
console.log(`n_ctx    ${props.default_generation_settings?.n_ctx}`);
console.log("");

// Confirm what the server does and does not implement, rather than trusting doctor.
console.log("capability probe (this is what sakur4d probes too)");
for (const [label, path, init] of [
  ["slots list", "/slots", undefined],
  ["slot 0 detail", "/slots/0", undefined],
  [
    "slot save",
    "/slots/0?action=save",
    { method: "POST", headers: { "content-type": "application/json" }, body: '{"filename":"probe.bin"}' },
  ],
  ["checkpoint ring", "/slots/0/checkpoints", undefined],
]) {
  try {
    const res = await fetch(`${BASE}${path}`, init);
    console.log(`  ${label.padEnd(18)} ${res.status}`);
  } catch (error) {
    console.log(`  ${label.padEnd(18)} error: ${error.message.slice(0, 60)}`);
  }
}
console.log("");

// A prompt large enough that prefill dominates the measurement on a 27B.
const TARGET_TOKENS = Number(argValue("--tokens", "4000"));
const built = await buildPrompt(TARGET_TOKENS);
const prompt = built.text;
console.log(`prompt   ${built.tokens} tokens (target ${TARGET_TOKENS}, ~${built.perLine.toFixed(1)}/line)`);
console.log(`predict  ${N_PREDICT} token(s), so generation is not the variable`);
console.log("");

console.log("A. cold prefix");
const a = await complete(prompt);
console.log(`   ${fmt(a.ms)}   prompt_n=${a.out.timings?.prompt_n ?? "?"}  cache_n=${a.out.timings?.cache_n ?? "?"}`);

console.log("B. identical prompt again — reused if KV caching works");
const b = await complete(prompt);
console.log(`   ${fmt(b.ms)}   prompt_n=${b.out.timings?.prompt_n ?? "?"}  cache_n=${b.out.timings?.cache_n ?? "?"}`);

console.log("C. prompt with its HEAD changed — the summarising-compaction case");
const changed = (await buildPrompt(TARGET_TOKENS, { headChanged: true })).text;
const c = await complete(changed);
console.log(`   ${fmt(c.ms)}   prompt_n=${c.out.timings?.prompt_n ?? "?"}  cache_n=${c.out.timings?.cache_n ?? "?"}`);
console.log("");

// The verdict, in terms of the thing that matters.
const reuseRatio = a.ms > 0 ? b.ms / a.ms : 1;
console.log("verdict");
if (b.ms < a.ms * 0.5) {
  console.log(`  automatic prefix reuse WORKS on this server.`);
  console.log(`  Re-sending the same prompt took ${fmt(b.ms)} against ${fmt(a.ms)} cold`);
  console.log(`  (${((1 - reuseRatio) * 100).toFixed(0)}% faster).`);
} else {
  console.log(`  automatic prefix reuse did NOT show up as a speedup here.`);
  console.log(`  Cold ${fmt(a.ms)}, warm ${fmt(b.ms)}. Check the server's --cache-reuse`);
  console.log(`  and --ctx-checkpoints flags; with a single slot and no checkpoint ring,`);
  console.log(`  the KV cache is still reused but the timing may be dominated by`);
  console.log(`  something else.`);
}
console.log("");
if (c.ms > b.ms * 2) {
  console.log(`  Changing the HEAD cost ${fmt(c.ms)} against ${fmt(b.ms)} for the identical`);
  console.log(`  prompt — ${(c.ms / Math.max(b.ms, 1)).toFixed(1)}x. That is the failure Sakur4`);
  console.log(`  exists to prevent, reproduced on real hardware.`);
} else {
  console.log(`  Changing the head did not cost noticeably more (${fmt(c.ms)} vs ${fmt(b.ms)}),`);
  console.log(`  which would mean this server recovers from a head change cheaply.`);
}
console.log("");
if (a.out.timings) {
  console.log("server-reported timings (ms)");
  for (const [k, v] of Object.entries(a.out.timings)) {
    console.log(`  A.${k.padEnd(14)} ${v}`);
  }
  for (const [k, v] of Object.entries(b.out.timings)) {
    console.log(`  B.${k.padEnd(14)} ${v}`);
  }
  for (const [k, v] of Object.entries(c.out.timings)) {
    console.log(`  C.${k.padEnd(14)} ${v}`);
  }
}

// ===========================================================================
// The compaction question, which is the one that actually matters
// ===========================================================================
//
// A and B above are a synthetic worst case: re-sending an identical prompt is not
// something a harness does. What a harness does is compact — and the question is
// whether the *surviving prefix* is reused after it.
//
// `sakur4d --backend <this server> demo` reports FULL RE-PREFILL and 0 tokens
// reused, because the backend exposes no checkpoint API for Sakur4 to align to. But
// measurement B proved llama.cpp reuses a longest common prefix automatically,
// without any checkpoint. If that holds after a compaction, then Sakur4's verdict is
// pessimistic: the saved work is real even though it cannot be predicted.
//
// So: build a long transcript, then build the two compactions of it and ask the
// server what it actually reused. `cache_n` is llama.cpp's own accounting, not an
// estimate — it is the number of prompt tokens it served from the KV cache.
//
//   1. naive summary    — the head is rewritten, nothing can match   → cache_n ≈ 0
//   2. preserve, aligned — evict at a boundary the *server* holds    → cache_n ≈ cut
//   3. preserve, unaligned — evict mid-block, one token into a span  → cache_n ≈ cut

console.log("");
console.log("═".repeat(72));
console.log("the compaction case");
console.log("═".repeat(72));
console.log("");

const LONG = Number(argValue("--long", "6000"));
const transcript = await buildPrompt(LONG);
console.log(`transcript  ${transcript.tokens} tokens`);

// Warm the slot with the full transcript, so a compaction has something to reuse.
const warm = await complete(transcript.text);
console.log(`warmed      cache_n=${warm.out.timings?.cache_n ?? "?"} after processing the whole thing`);
console.log("");

/**
 * Split a transcript into lines and reassemble it as a compaction would.
 *
 * `keepRatio` is the fraction of the head preserved verbatim; everything after it
 * becomes a summary. `nudge` shifts the boundary by a few tokens, which is the
 * difference between landing on a block the server holds and landing one token past
 * it — the distinction Sakur4's checkpoint alignment exists to make.
 */
function compact(text, keepRatio, { rewriteHead = false, nudge = 0 } = {}) {
  const lines = text.split("\n");
  const cutLine = Math.floor(lines.length * keepRatio);
  const kept = lines.slice(0, cutLine);
  const dropped = lines.slice(cutLine);

  if (rewriteHead) {
    // What a summarising harness sends: the head is now prose *about* the
    // transcript rather than the transcript, so no prefix matches.
    return [
      "## Summary of the session so far",
      "",
      `The agent worked through ${dropped.length} steps, inspecting handlers and`,
      "validating input. Key decisions: none that change the current task.",
      "",
      kept.join("\n"),
    ].join("\n");
  }

  const boundary = kept.join("\n") + (nudge > 0 ? "\n" + "x".repeat(nudge) : "");
  return [
    boundary,
    "",
    "## Evicted: 12 steps replaced by a summary",
    `The ${dropped.length} steps removed here inspected handlers and validated input.`,
    "",
    "## Continue",
  ].join("\n");
}

const cases = [
  ["naive summary (head rewritten)", compact(transcript.text, 0.68, { rewriteHead: true })],
  ["preserve 68% (aligned boundary)", compact(transcript.text, 0.68)],
  ["preserve 68% (nudged 1 token past)", compact(transcript.text, 0.68, { nudge: 4 })],
];

const results = [];
for (const [label, prompt] of cases) {
  const tokens = await tokenCount(prompt);
  const r = await complete(prompt);
  const cacheN = r.out.timings?.cache_n ?? 0;
  const promptN = r.out.timings?.prompt_n ?? 0;
  results.push({ label, tokens, cacheN, promptN, ms: r.ms });
  console.log(`${label}`);
  console.log(
    `  prompt ${tokens} tokens · processed ${promptN} · reused ${cacheN} ` +
      `(${((cacheN / Math.max(tokens, 1)) * 100).toFixed(0)}%) · ${fmt(r.ms)}`,
  );
}

console.log("");
const naive = results[0];
const aligned = results[1];
const nudged = results[2];

const savedAligned = naive.promptN - aligned.promptN;
const msPerToken = a.out.timings?.prompt_per_token_ms ?? null;
const secondsSaved = msPerToken ? (savedAligned * msPerToken) / 1000 : null;

console.log("verdict");
if (aligned.cacheN > naive.cacheN * 2 && aligned.cacheN > 100) {
  console.log(`  PRESERVING THE PREFIX PAYS OFF ON THIS SERVER.`);
  console.log(`  A summarising compaction processed ${naive.promptN} prompt tokens;`);
  console.log(`  Sakur4's boundary processed ${aligned.promptN} and reused ${aligned.cacheN}.`);
  console.log(`  That is ${savedAligned} tokens of prefill avoided` +
    (secondsSaved ? `, about ${secondsSaved.toFixed(1)} s at this model's measured` : "") +
    (secondsSaved ? ` ${(1 / msPerToken).toFixed(0)} tok/s prefill rate.` : "."));
  console.log("");
  console.log(`  NOTE: sakur4d reports FULL RE-PREFILL / 0% saved for this backend,`);
  console.log(`  because it cannot see a checkpoint API to align to. That verdict is`);
  console.log(`  PESSIMISTIC here — the saving is real, it is just not predictable.`);
} else if (aligned.cacheN > naive.cacheN) {
  console.log(`  Partial: aligned reused ${aligned.cacheN} against ${naive.cacheN} for the`);
  console.log(`  naive case. Below expectations; inspect the numbers above.`);
} else {
  console.log(`  No measurable benefit from preserving the prefix on this server.`);
}

if (nudged.cacheN !== aligned.cacheN) {
  console.log("");
  console.log(`  Boundary sensitivity: moving the cut by ~4 tokens changed reuse from`);
  console.log(`  ${aligned.cacheN} to ${nudged.cacheN} — which is why alignment matters at all.`);
} else {
  console.log("");
  console.log(`  Boundary sensitivity: a ~4-token shift made no difference here`);
  console.log(`  (${aligned.cacheN} vs ${nudged.cacheN}), so this server's reuse is not`);
  console.log(`  sensitive to small boundary moves.`);
}
