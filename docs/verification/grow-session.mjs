#!/usr/bin/env node
/**
 * Grow one transcript turn by turn and report what the upstream is actually asked to
 * process after each step.
 *
 * # Why this exists
 *
 * Every other check asks "did the proxy rewrite?" — a yes or no. This asks **how much
 * context survived**, which is the question a user cares about and the one that exposed the
 * planner overshooting its own target: at both a 32k and an 82k window the prompt settled at
 * ~3,412 tokens, so the amount of context kept did not depend on the window at all.
 *
 * The number to watch is the upstream's own `prompt_tokens`, not anything the proxy reports.
 * The proxy's view of what it sent is the thing under test; the model server's count is
 * evidence.
 *
 * Usage:
 *   node docs/verification/grow-session.mjs --proxy http://127.0.0.1:8096 [--steps 600]
 */

const argv = process.argv.slice(2);
const arg = (name, fallback) => {
  const i = argv.indexOf(`--${name}`);
  return i >= 0 && argv[i + 1] && !argv[i + 1].startsWith("--") ? argv[i + 1] : fallback;
};

const PROXY = arg("proxy", "http://127.0.0.1:8096");
const STEPS = Number(arg("steps", "600"));
const EVERY = Number(arg("every", "150"));

const ask = (i) =>
  `turn ${i}: inspect src/module_${i}/handler.rs and explain what it validates. Include the ` +
  "error paths and recovery behaviour in your answer. Also mention how the retry budget is " +
  "configured and what happens when it is exhausted, because that is the part that has caused " +
  "incidents before.";
const answer = (i) =>
  `turn ${i}: module_${i} validates its input and returns a Result. The error path propagates ` +
  "with context, and recovery retries once with a backoff. The retry budget is read from config " +
  "at startup and defaults to three attempts, after which the error is surfaced to the caller " +
  "unchanged.";

async function main() {
  const health = await fetch(`${PROXY}/v1/models`).catch(() => null);
  if (!health?.ok) {
    console.error(`grow-session: no proxy at ${PROXY}`);
    process.exit(2);
  }

  console.log(`grow-session against ${PROXY}`);
  console.log("");
  console.log("  messages sent   upstream prompt_tokens   verdict");
  console.log("  ─────────────────────────────────────────────────────────────");

  const messages = [{ role: "system", content: "You are a coding agent." }];
  let previous = null;
  const rows = [];

  for (let i = 0; i < STEPS; i += 1) {
    messages.push({ role: "user", content: ask(i) });
    messages.push({ role: "assistant", content: answer(i) });

    if (i % EVERY !== EVERY - 1) continue;

    messages.push({ role: "user", content: "continue" });
    const response = await fetch(`${PROXY}/v1/chat/completions`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ model: "a 27B model", messages, max_tokens: 2, temperature: 0 }),
    });
    const body = await response.json().catch(() => ({}));
    const prompt = body?.usage?.prompt_tokens ?? null;

    // A drop means the proxy trimmed. Flat after a drop means the size it settles at, which
    // is the number that should track the target and does not.
    const verdict =
      prompt === null
        ? "no usage reported"
        : previous === null
          ? ""
          : prompt < previous * 0.7
            ? "\x1b[33mTRIMMED\x1b[0m"
            : Math.abs(prompt - previous) < 50
              ? "\x1b[36mSETTLED\x1b[0m"
              : "";
    console.log(
      `  ${String(messages.length).padStart(14)}   ${String(prompt ?? "?").padStart(19)}   ${verdict}`,
    );
    rows.push({ messages: messages.length, promptTokens: prompt });
    previous = prompt;
    messages.pop();
  }

  const settled = rows.filter((r, i) => i > 0 && rows[i - 1].promptTokens !== null && Math.abs(r.promptTokens - rows[i - 1].promptTokens) < 50);
  console.log("");
  if (settled.length > 0) {
    console.log(
      `  Settled at ${settled.at(-1).promptTokens} tokens with ${settled.at(-1).messages} messages in the transcript.`,
    );
    console.log(
      "  A window-relative target should make this number scale with the proxy's\n" +
        "  --context-window. If it is the same at two different windows, the target is not\n" +
        "  what is bounding the result.",
    );
  } else {
    console.log("  Never settled: the transcript grew without ever being trimmed.");
  }
}

main().catch((error) => {
  console.error(`grow-session: ${error.message}`);
  process.exit(1);
});
