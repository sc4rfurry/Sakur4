// The pass/fail/indeterminate decision for a latency check, as a pure function.
//
// # Why this is a module rather than four lines inside the benchmark
//
// `nfr2-recall.mjs` reported **1352.9 ms** in a full `verify.mjs` run and **23.0 ms** when re-run on its
// own. Nothing about Sakur4 changed; 261 tests and a clippy build were still finishing. The figure
// measured the machine.
//
// The fix is to report three outcomes instead of two, and to say which one applies — but the decision is
// `overTarget && contended`, and **in practice `contended` is nearly always true on a developer machine**
// (this one reports 42.8 core-seconds per second on 8 cores while idle-ish, because the measurement is
// cumulative CPU time over elapsed time and a compiler thread pool alone exceeds the core count).
//
// So the branch that matters is reachable in principle and almost unreachable in practice, which is the
// worst position for a guard to be in: it looks like protection and exercises nothing. Extracting the
// decision into a pure function is what makes it testable — `verdict.mjs` asserts every combination,
// including the ones a live run will not produce.
//
// The precedent is in `crates/sakur4-testkit`: pure decision functions separated from the harness that
// drives them, for exactly this reason. A four-line inline conditional has no such option, which is why
// it went untested and why the false failure reached a real run.

/// Did the machine have more work queued than it has cores?
///
/// `os.loadavg()` is the run-queue average and is meaningful on Linux and macOS; it is all zeros on
/// Windows, where this falls back to comparing consumed CPU time against elapsed wall time — no privilege
/// needed, and the same question: was more than one core's worth of work happening per second?
///
/// `cpuSeconds` is **seconds of CPU time**, not milliseconds. An earlier version divided by 1000 as well,
/// which made the fallback report eight times less contention than there was; the unit is named in the
/// parameter so the next person does not have to read the arithmetic to find out.
///
/// The fallback over-reports on a machine that is merely compiling, which is why `contended` alone is
/// never a verdict. It only downgrades a FAIL to INCONCLUSIVE, and a PASS is a PASS either way.
export function isContended({ loadavg, cores, cpuSeconds, wallMs }) {
  const safeCores = cores || 1;
  const load = loadavg?.[0] ?? 0;
  if (load > 0) {
    return { contended: load > safeCores, detail: `load ${load.toFixed(2)} on ${safeCores} cores` };
  }
  const perSecond = cpuSeconds / Math.max(1, wallMs / 1000);
  return {
    contended: perSecond > safeCores * 0.9,
    detail: `${perSecond.toFixed(2)} core-seconds per second on ${safeCores} cores`,
  };
}

/// Three outcomes, because "outside the target" and "the machine was busy" are different facts.
///
/// * `PASS` — inside the target. A green result.
/// * `FAIL` — outside the target on a machine that was **not** overloaded. A real regression.
/// * `INCONCLUSIVE` — outside the target while contended. Neither, and the caller must not report it as
///   one: `verify.mjs` records it as a skip carrying the reason, so the run claims neither a number it
///   did not earn nor a regression that did not happen.
export function decideVerdict({ worstMs, targetMs, contended }) {
  if (worstMs < targetMs) return "PASS";
  return contended ? "INCONCLUSIVE" : "FAIL";
}
