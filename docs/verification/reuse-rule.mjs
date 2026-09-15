// Pin down the exact reuse rule on this llama.cpp build.
//
// # What the evidence so far says
//
// From `reuse-probe.mjs`, two facts that together explain every earlier surprise:
//
//   * A prompt that is a **strict prefix** of the resident context reuses *nothing*
//     (`cache_n = 0` for a 2153-token head sent alone, and for a fresh copy of the
//     whole transcript before the slot had processed anything new).
//   * A prompt that shares a prefix **and has new material after it** reuses that
//     prefix, minus a margin: sending the same 2153-token head plus a 3-token marker
//     reused 2153 and processed 3.
//
// So reuse needs something to be *new*. That is not a quirk — it is coherent: with
// no new tokens there is no next-token position to evaluate, so the server keeps the
// cached state and returns without doing prefill work at all.
//
// What remains unclear is the cliff between diverging at sentence 158 (reuse 0) and
// 159 (reuse 3865). Two candidate explanations:
//
//   A. The rule is about **where** the divergence is — near the end, the remaining
//      tail is short enough that there is nothing worth reusing.
//   B. The rule is about **how much new material** follows — below some size, the
//      server reprocesses from scratch rather than paying to reuse.
//
// These make different predictions, and the experiment separates them by holding one
// variable fixed while moving the other:
//
//   * Fix the divergence *position*, vary the *tail length*.
//   * Fix the *tail length*, vary the divergence *position*.
//
// If reuse appears as soon as the tail exceeds some size, it is B. If it depends on
// the position even with a long tail, it is A.
//
// Run: node docs/verification/reuse-rule.mjs [--base http://host:port]

const BASE = argValue("--base", process.env.LLAMA_BASE ?? "http://your-llama-server:8080");

function argValue(name, fallback) {
  const i = process.argv.indexOf(name);
  return i >= 0 && process.argv[i + 1] ? process.argv[i + 1] : fallback;
}

async function json(path, init) {
  const res = await fetch(`${BASE}${path}`, init);
  const text = await res.text();
  if (!res.ok) throw new Error(`${path} -> ${res.status}: ${text.slice(0, 200)}`);
  return JSON.parse(text);
}

const tokenize = async (content) =>
  (await json("/tokenize", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ content }),
  })).tokens;

async function complete(prompt) {
  const res = await json("/completion", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ prompt, n_predict: 1, temperature: 0, cache_prompt: true, stream: false }),
  });
  return res.timings ?? {};
}

/** A long, self-dissimilar body of text used as the resident context. */
function body(sentences) {
  return Array.from(
    { length: sentences },
    (_, n) =>
      `Entry ${n} records the observation that subsystem ${n * 37 % 991} produced ` +
      `output ${n * 7919 % 99991} while handling request ${n * 13 % 9973}.`,
  ).join(" ");
}

/**
 * Padding used as "new material after the divergence".
 *
 * # Why the size is verified rather than assumed
 *
 * The first version emitted `pad<N>` for N in a range and hoped for roughly one
 * token each. It produced far more, and section B's prompts silently overflowed the
 * server's context — which made every one of those rows meaningless (the server
 * truncated, destroying any shared prefix, and reported `prompt_n` in the tens of
 * thousands for a 5,952-token window).
 *
 * So this counts what it built. A prompt that does not fit the window is not a
 * measurement of cache reuse, it is a measurement of truncation.
 */
async function filler(tokens) {
  const pieces = [];
  let n = 0;
  // Build, measuring as we go, until the token budget is met. Each piece is a
  // distinct short word so the token count grows predictably and no two pieces
  // collapse to the same token id.
  while (true) {
    pieces.push(`pad${(n * 48271) % 99991}`);
    n++;
    if (n % 32 === 0 || pieces.length >= tokens) {
      const count = (await tokenize(pieces.join(" "))).length;
      if (count >= tokens) return { text: pieces.join(" "), count };
    }
  }
}

const CONTEXT = 130;
const resident = body(CONTEXT);
const residentTokens = await tokenize(resident);
const WINDOW = (await json("/props")).default_generation_settings?.n_ctx ?? 0;
console.log(`server            ${BASE}`);
console.log(`server window     ${WINDOW} tokens`);
console.log(`resident context  ${residentTokens.length} tokens`);
if (WINDOW && residentTokens.length > WINDOW * 0.9) {
  throw new Error(
    `the resident context (${residentTokens.length}) is too close to the window ` +
      `(${WINDOW}); variants would overflow and every measurement would be of ` +
      `truncation rather than of reuse`,
  );
}
console.log("");

await complete(resident);
// Confirm the slot is warm, so a miss below is a statement about the rule and not
// about a cold server.
const confirm = await complete(`${resident} tail`);
console.log(`warm check        cache_n=${confirm.cache_n} (${residentTokens.length} expected)`);
console.log("");

const sentences = resident.split(" ");
/** Build a prompt that shares the first `keepWords` words and then adds `padTokens`. */
async function variant(keepWords, padTokens) {
  const head = sentences.slice(0, keepWords).join(" ");
  const pad = padTokens > 0 ? (await filler(padTokens)).text : "";
  const full = pad ? `${head} ${pad}` : head;
  const tokens = await tokenize(full);
  if (WINDOW && tokens.length > WINDOW * 0.9) {
    throw new Error(
      `variant of ${tokens.length} tokens would overflow the ${WINDOW}-token window`,
    );
  }
  return { full, tokens };
}

// ---------------------------------------------------------------------------
// A · fix the position, vary the tail
// ---------------------------------------------------------------------------
console.log("A · divergence position FIXED, tail length varied");
console.log(`    (head ≈ ${Math.floor(sentences.length * 0.6)} of ${sentences.length} words)`);
console.log("");
console.log("    pad tokens   prompt tokens   cache_n   prompt_n   reused %");
const keepWords = Math.floor(sentences.length * 0.6);
const headTokens = (await tokenize(sentences.slice(0, keepWords).join(" "))).length;
for (const pad of [0, 1, 2, 3, 4, 8, 32, 128, 512]) {
  const { full, tokens } = await variant(keepWords, pad);
  const t = await complete(full);
  const pct = ((t.cache_n ?? 0) / Math.max(tokens.length, 1)) * 100;
  console.log(
    `    ${String(pad).padStart(10)}   ${String(tokens.length).padStart(13)}   ` +
      `${String(t.cache_n ?? "?").padStart(7)}   ${String(t.prompt_n ?? "?").padStart(8)}   ` +
      `${pct.toFixed(0).padStart(6)}%`,
  );
}
console.log(`    (head alone is ${headTokens} tokens)`);
console.log("");

// ---------------------------------------------------------------------------
// B · fix the tail, vary the position
// ---------------------------------------------------------------------------
console.log("B · tail length FIXED, divergence position varied");
console.log("");
console.log("    kept words   kept tokens   prompt tokens   cache_n   prompt_n   reused %");
const TAIL = 512;
for (const fraction of [0.1, 0.3, 0.5, 0.7, 0.9]) {
  const k = Math.floor(sentences.length * fraction);
  const { full, tokens } = await variant(k, TAIL);
  const kept = (await tokenize(sentences.slice(0, k).join(" "))).length;
  const t = await complete(full);
  const pct = ((t.cache_n ?? 0) / Math.max(tokens.length, 1)) * 100;
  console.log(
    `    ${String(k).padStart(10)}   ${String(kept).padStart(11)}   ` +
      `${String(tokens.length).padStart(13)}   ${String(t.cache_n ?? "?").padStart(7)}   ` +
      `${String(t.prompt_n ?? "?").padStart(8)}   ${pct.toFixed(0).padStart(6)}%`,
  );
}
console.log("");

// ---------------------------------------------------------------------------
// C · a strict prefix, with a growing tail of one token at a time
// ---------------------------------------------------------------------------
console.log("C · the strict-prefix boundary, one token at a time");
console.log("");
console.log("    tail tokens   prompt tokens   cache_n   prompt_n");
for (const pad of [0, 1, 2, 3, 4, 6]) {
  const padText = pad > 0 ? ` ${(await filler(pad)).text}` : "";
  const prompt = resident + padText;
  const tokens = await tokenize(prompt);
  const t = await complete(prompt);
  console.log(
    `    ${String(pad).padStart(11)}   ${String(tokens.length).padStart(13)}   ` +
      `${String(t.cache_n ?? "?").padStart(7)}   ${String(t.prompt_n ?? "?").padStart(8)}`,
  );
}

console.log("");
console.log("reading the result");
console.log("  Section A holds the position fixed, so a change in reuse as the tail grows");
console.log("  is a statement about tail size. Section B holds the tail fixed, so a change");
console.log("  as the kept prefix grows is a statement about position. If A turns on and B");
console.log("  scales, reuse equals the shared prefix minus a boundary margin — which is");
console.log("  the number Sakur4's eviction boundary has to respect.");
