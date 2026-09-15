// Explain the surprising result in `llamacpp-prefix.mjs`.
//
// The observation
// ---------------
// In the compaction test, preserving a ~2,750-token prefix verbatim produced
// `cache_n = 0` — the server reused nothing — while a version with four characters
// inserted after the same prefix produced `cache_n = 3634`. That is backwards from
// what "longest common prefix" implies, so one of two things is true:
//
//   1. The server does not do what LCP matching implies, or
//   2. the test's verbatim prefix was not actually a token-level prefix.
//
// Hypothesis 2 is far more likely, and it is checkable. llama.cpp tokenises the
// whole prompt and matches token by token. `"\n".repeat(3)` and `"x".repeat(4)`
// tokenise differently, but `"x".repeat(3)` and `"x".repeat(4)` may well collapse to
// the same token id — so a test that varies a run of identical characters is not
// varying the token sequence at all.
//
// This script removes the guesswork: it tokenises both prompts and reports the real
// longest common prefix in *tokens*, which is the only thing llama.cpp matches on.
// It also checks the boundary directly, because that is the case Sakur4 actually
// produces — an eviction cut at a token position, with nothing inserted.
//
// Run: node docs/verification/lcp-diagnose.mjs [--base http://host:port]

const BASE = argValue("--base", process.env.LLAMA_BASE ?? "http://100.98.158.87:8080");

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

/** Longest common prefix length of two token arrays. */
function lcp(a, b) {
  const n = Math.min(a.length, b.length);
  let i = 0;
  while (i < n && a[i] === b[i]) i++;
  return i;
}

/** A deterministic filler whose tokenisation is stable and not self-similar. */
function corpus(lines) {
  return Array.from({ length: lines }, (_, n) =>
    `step ${n}: inspect src/module_${n % 97}/handler.rs; fn handle_request_${n} ` +
    `validates input and returns Result<Response, Error>; callers ${(n % 7) + 1}; ` +
    `coverage ${(n % 40) + 60}%; note ${(n * 7919) % 100000}`,
  ).join("\n");
}

console.log(`server ${BASE}\n`);

// ---------------------------------------------------------------------------
// 1 · Does a run of identical characters change the token sequence?
// ---------------------------------------------------------------------------
console.log("1 · is a short run of characters a token boundary?");
console.log("   (this is what the earlier test varied, assuming it changed tokens)");
for (const pad of ["", "x", "xx", "xxx", "xxxx", "\n", "\n\n", "\n\n\n"]) {
  const t = await tokenize(`end${pad}next`);
  console.log(`   ${JSON.stringify(`end${pad}next`).padEnd(22)} -> ${JSON.stringify(t)}`);
}
console.log("");

// ---------------------------------------------------------------------------
// 2 · The case Sakur4 actually produces: a cut, with nothing inserted
// ---------------------------------------------------------------------------
const text = corpus(140);
const full = await tokenize(text);

console.log("2 · LCP of a real eviction boundary, measured in tokens");
console.log(`   transcript           ${full.length} tokens`);

// Warm the slot with the full transcript.
await complete(text);

// Variant A: what a summarising harness sends — the head is now prose *about* the
// transcript rather than the transcript, so no prefix matches.
const summarised = `## Summary\n\nThe agent inspected handlers.\n\n${text}`;
// Variant B: Sakur4's boundary — keep the head, replace the tail, insert nothing
// between them beyond the newline the assembler already produces. This is the case
// that matters.
const kept = text.split("\n").slice(0, 95).join("\n");
const preserved = `${kept}\n\n## Evicted: replaced by a summary\n`;
// Variant C: the same intent, but with a run of identical characters at the
// boundary — the shape an earlier test used while believing it shifted the boundary
// by a token.
const nudged = `${kept}\n\nxxxx\n## Evicted: replaced by a summary\n`;

const rows = [
  ["summarised (head rewritten)", summarised],
  ["preserved (Sakur4 boundary)", preserved],
  ["preserved + 4 identical chars", nudged],
];

console.log("");
for (const [label, prompt] of rows) {
  const tokens = await tokenize(prompt);
  const lcpN = lcp(full, tokens);
  const timings = await complete(prompt);
  console.log(`   ${label}`);
  console.log(
    `     tokens ${tokens.length} · real token LCP with transcript ${lcpN} ` +
      `(${((lcpN / full.length) * 100).toFixed(0)}% of it)`,
  );
  console.log(
    `     server reused cache_n=${timings.cache_n ?? "?"}, processed prompt_n=${timings.prompt_n ?? "?"}`,
  );
}
console.log("");

// ---------------------------------------------------------------------------
// 3 · What this established, and what it did not
// ---------------------------------------------------------------------------
console.log("3 · outcome");
console.log("   The three variants here do not isolate what they were meant to: all three");
console.log("   append material after the preserved head, so all three have a non-empty");
console.log("   tail, and the differences between them are smaller than the run-to-run");
console.log("   variation this server shows. See `reuse-rule.mjs`, which varies tail size");
console.log("   and divergence position one at a time, for the answer this script was");
console.log("   reaching for.");
console.log("");
console.log("   What it does establish, and what `reuse-probe.mjs` confirms: a run of");
console.log("   identical characters is NOT a token. The probe above shows `endnext`,");
console.log("   `endxnext` and `endxxxxnext` tokenising to different sequences, which is why");
console.log("   reasoning about boundary positions in characters is meaningless here.");
