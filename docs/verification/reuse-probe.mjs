// Controlled experiment: what does this llama.cpp build actually reuse?
//
// # Why this exists
//
// The previous two scripts produced a contradiction. Two prompts with the *same*
// measured token-level longest common prefix (4419 tokens) against the resident
// context got opposite results — one reused 0, the other reused 3913. "Longest
// common prefix" alone cannot explain that, so something else is in the decision.
//
// Guessing at it would be worthless. This varies one thing at a time:
//
//   1. **Position of the first divergence.** Change a single word at token N and
//      see how many tokens are reused. If reuse tracks N, matching is by LCP. If it
//      quantises to some block size, it is not.
//   2. **Divergence at the very end vs. the middle.** A change at the last token
//      against one at token 100 tells apart "reuse everything before the change"
//      from "reuse nothing unless there is a long new tail".
//   3. **Order of requests.** The same prompt issued before and after an
//      intermediate request, to rule out the slot being invalidated by whatever ran
//      in between — which is the one explanation that would make the earlier
//      contradiction an artefact of test sequencing rather than of the server.
//
// `cache_n` is llama.cpp's own count of prompt tokens served from the KV cache, and
// `prompt_n` is what it actually evaluated. They sum to the prompt length, so the
// pair is a complete accounting rather than an estimate.
//
// Run: node docs/verification/reuse-probe.mjs [--base http://host:port]

const BASE = argValue("--base", process.env.LLAMA_BASE ?? null);
// Required, with no default. A private endpoint's address does not belong in a repository, and a
// default pointing at somebody's machine is worse than none: it works for its owner and fails for
// everyone else, without saying that a flag was expected.
if (!BASE) {
  console.error("usage: --base http://host:port   (or set LLAMA_BASE)");
  process.exit(2);
}

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

function lcp(a, b) {
  const n = Math.min(a.length, b.length);
  let i = 0;
  while (i < n && a[i] === b[i]) i++;
  return i;
}

/**
 * A sentence list where every sentence is distinct and long enough to be several
 * tokens, so changing one word changes exactly one region of the token stream.
 */
function sentences(count) {
  return Array.from(
    { length: count },
    (_, n) =>
      `Sentence ${n} describes module ${n * 31 % 997} which validates the incoming ` +
      `payload and returns a structured result to its caller number ${(n % 11) + 1}.`,
  );
}

/** Replace one word in sentence `at`, leaving every other sentence byte-identical. */
function withChange(all, at) {
  const copy = [...all];
  copy[at] = copy[at].replace("validates", "rejects");
  return copy;
}

const SENTENCES = 160;
const base = sentences(SENTENCES);
const baseText = base.join(" ");
const baseTokens = await tokenize(baseText);

console.log(`server            ${BASE}`);
console.log(`base transcript   ${baseTokens.length} tokens, ${SENTENCES} sentences`);
console.log("");

// ---------------------------------------------------------------------------
// Warm the slot, then establish that a plain re-send is reused.
// ---------------------------------------------------------------------------
const warm = await complete(baseText);
console.log(`warm              cache_n=${warm.cache_n}  prompt_n=${warm.prompt_n}`);

const resend = await complete(baseText);
console.log(`re-send identical cache_n=${resend.cache_n}  prompt_n=${resend.prompt_n}`);
console.log("");

// ---------------------------------------------------------------------------
// 1 · Divergence position sweep
// ---------------------------------------------------------------------------
console.log("1 · reuse against the position of the first change");
console.log("   (the prompt shares everything before the changed sentence, and nothing after)");
console.log("");
console.log("   changed at   true LCP   cache_n   prompt_n   reuse %");

const positions = [1, 10, 40, 80, 120, 158, 159];
const sweep = [];
for (const at of positions) {
  const variant = withChange(base, at).join(" ");
  const variantTokens = await tokenize(variant);
  const trueLcp = lcp(baseTokens, variantTokens);
  const t = await complete(variant);
  const reuse = ((t.cache_n ?? 0) / variantTokens.length) * 100;
  sweep.push({ at, trueLcp, cacheN: t.cache_n ?? 0, promptN: t.prompt_n ?? 0, trueLcp2: trueLcp });
  console.log(
    `   ${String(at).padStart(8)}   ${String(trueLcp).padStart(8)}   ` +
      `${String(t.cache_n ?? "?").padStart(7)}   ${String(t.prompt_n ?? "?").padStart(8)}   ` +
      `${reuse.toFixed(0).padStart(5)}%`,
  );
}

console.log("");

// ---------------------------------------------------------------------------
// 2 · Does anything after the prefix matter?
// ---------------------------------------------------------------------------
console.log("2 · same divergence, different tails");
const cut = sweep.find((s) => s.at === 80);
const head = base.slice(0, 80).join(" ");
const tails = [
  ["nothing (head only)", ""],
  ["a short marker", "\n\n## Continue"],
  ["a longer summary", "\n\n## Evicted\n\nTwelve steps were removed and replaced by this summary of what they did."],
];
for (const [label, tail] of tails) {
  const prompt = head + tail;
  const tokens = await tokenize(prompt);
  const t = await complete(prompt);
  console.log(
    `   ${label.padEnd(20)} tokens ${String(tokens.length).padStart(5)}  ` +
      `cache_n ${String(t.cache_n ?? "?").padStart(5)}  prompt_n ${String(t.prompt_n ?? "?").padStart(5)}  ` +
      `(true LCP ${lcp(baseTokens, tokens)})`,
  );
}
console.log("");

// ---------------------------------------------------------------------------
// 3 · Order sensitivity
// ---------------------------------------------------------------------------
console.log("3 · is reuse affected by what ran in between?");
const probe = withChange(base, 40).join(" ");
const first = await complete(probe);
const between = await complete(baseText);
const second = await complete(probe);
console.log(`   variant alone        cache_n=${first.cache_n}  prompt_n=${first.prompt_n}`);
console.log(`   (base re-sent)       cache_n=${between.cache_n}`);
console.log(`   variant again        cache_n=${second.cache_n}  prompt_n=${second.prompt_n}`);
console.log("");

// ---------------------------------------------------------------------------
// Verdict
// ---------------------------------------------------------------------------
const monotone = sweep.every((s, i) => i === 0 || s.cacheN >= sweep[i - 1].cacheN);
const nearLcp = sweep.every((s) => Math.abs(s.cacheN - s.trueLcp) <= Math.max(64, s.trueLcp * 0.05));

console.log("verdict");
if (nearLcp && monotone) {
  console.log("  Reuse tracks the true token LCP, and grows with it. This server");
  console.log("  behaves as longest-common-prefix matching implies.");
} else if (monotone) {
  console.log("  Reuse grows with the divergence position but does not equal the true");
  console.log("  token LCP — there is quantisation or a safety margin at the boundary.");
  console.log("  The direction is what matters for Sakur4: preserving more prefix reuses");
  console.log("  more, which is the property its eviction is built on.");
} else {
  console.log("  Reuse does not grow monotonically with the divergence position. Something");
  console.log("  other than prefix length is deciding, and the numbers above are the record");
  console.log("  of it.");
}
console.log("");
console.log(`  Order sensitivity: the same variant reused ${first.cache_n} alone and`);
console.log(`  ${second.cache_n} after the base was re-sent in between. ` +
  (first.cacheN === second.cacheN
    ? "No order effect."
    : "There IS an order effect, which would explain the earlier contradiction."));
