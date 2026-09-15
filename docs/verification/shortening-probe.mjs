// Isolate the one case that did not fit: a shortened prompt, sent right after a
// longer one.
//
// # The contradiction to resolve
//
// `reuse-rule.mjs` produced two results that cannot both be true of a plain
// prefix-matching cache:
//
//   * Section C — the resident context plus one new token reused all 3,800 tokens.
//   * Section B — the first 349 tokens of the same context plus 512 tokens of new
//     material reused nothing at all.
//
// In both, the prompt *starts with* the resident context. The difference is that C
// extends it while B truncates it and appends unrelated text. If reuse were LCP
// matching against the resident KV, both should reuse, and B should reuse 349.
//
// # The candidate explanation, and how to falsify it
//
// B ran immediately after A, whose last request was a 2,785-token prompt that was
// itself a truncation of the resident context. If the slot's KV state is the *last
// request's* rather than a persistent pool, then B — whose first 349 tokens match the
// resident but not necessarily the tail of A's last request — would legitimately miss.
//
// That is falsifiable. Send the exact same prompt twice with nothing in between:
//
//   * If the second send reuses, the cache holds whatever was last processed, and
//     section B's zeros were an artefact of what ran before it.
//   * If the second send also misses, then shortening a prompt genuinely defeats
//     reuse on this build, which would matter a great deal.
//
// Run: node docs/verification/shortening-probe.mjs [--base http://host:port]

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

function body(sentences) {
  return Array.from(
    { length: sentences },
    (_, n) =>
      `Entry ${n} records the observation that subsystem ${n * 37 % 991} produced ` +
      `output ${n * 7919 % 99991} while handling request ${n * 13 % 9973}.`,
  ).join(" ");
}

async function filler(tokens) {
  const pieces = [];
  let n = 0;
  while (true) {
    pieces.push(`pad${(n * 48271) % 99991}`);
    n++;
    if (n % 32 === 0 || pieces.length >= tokens) {
      const count = (await tokenize(pieces.join(" "))).length;
      if (count >= tokens) return pieces.join(" ");
    }
  }
}

const report = async (label, prompt) => {
  const tokens = await tokenize(prompt);
  const t = await complete(prompt);
  console.log(
    `  ${label.padEnd(34)} ${String(tokens.length).padStart(5)} tok  ` +
      `cache_n ${String(t.cache_n ?? "?").padStart(5)}  prompt_n ${String(t.prompt_n ?? "?").padStart(5)}`,
  );
  return t;
};

const resident = body(130);
const residentTokens = await tokenize(resident);
const words = resident.split(" ");

console.log(`server    ${BASE}`);
console.log(`resident  ${residentTokens.length} tokens\n`);

// ---------------------------------------------------------------------------
console.log("probe 1 · a shortened prompt, sent twice with nothing between");
console.log("");
const shortHead = words.slice(0, Math.floor(words.length * 0.2)).join(" ");
const shortPrompt = `${shortHead} ${await filler(400)}`;
await report("resident (warm)", resident);
await report("shortened, first send", shortPrompt);
await report("shortened, identical resend", shortPrompt);
console.log("");

// ---------------------------------------------------------------------------
console.log("probe 2 · the same, but re-warming the resident in between");
console.log("");
await report("resident (re-warm)", resident);
await report("shortened, right after warm", shortPrompt);
console.log("");

// ---------------------------------------------------------------------------
console.log("probe 3 · truncation without any appended material");
console.log("");
await report("resident (re-warm)", resident);
await report("resident truncated to 20%", shortHead);
await report("truncated, identical resend", shortHead);
console.log("");

// ---------------------------------------------------------------------------
console.log("probe 4 · extension, for contrast (the case section C covered)");
console.log("");
await report("resident (re-warm)", resident);
await report("resident + 400 new tokens", `${resident} ${await filler(400)}`);
await report("that prompt, identical resend", `${resident} ${await filler(400)}`);
console.log("");

console.log("reading the result");
console.log("  probe 1's resend is the decisive one. If it reuses, this build caches");
console.log("  whatever was last processed and section B's zeros came from request order,");
console.log("  not from shortening. If it misses, shortening defeats reuse here.");
