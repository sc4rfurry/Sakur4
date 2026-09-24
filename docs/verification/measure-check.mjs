#!/usr/bin/env node
// The latency-verdict decision, asserted across every combination it can take.
//
// # Why this file exists
//
// `nfr2-recall.mjs` gained an `INCONCLUSIVE` outcome so that a latency figure taken on a busy machine
// stops being reported as a regression — the exact failure that put `1352.9 ms` in a run where the true
// figure was `23.0 ms`.
//
// That branch is `overTarget && contended`, and on a real machine **`contended` is nearly always true**,
// so the branch is reachable in principle and almost unreachable in practice. A guard in that position is
// worse than none: it looks like protection and exercises nothing, which is the defect this project has
// now recorded for the unused-dependency scan, the uncalled-function scan, and the wiki link checker.
//
// So the decision was extracted into `measure.mjs` as a pure function and is asserted here, including the
// combinations a live run will not produce.
import { decideVerdict, isContended } from "./measure.mjs";

let failures = 0;
function check(name, actual, expected) {
  if (actual === expected) {
    console.log(`  ok    ${name}`);
  } else {
    console.log(`  FAIL  ${name} — expected ${expected}, got ${actual}`);
    failures += 1;
  }
}

console.log("  latency verdict");

// The four combinations of (inside target?) x (contended?). All four are asserted, which is the whole
// point of extracting the function: a live run can only reach three of them.
check("inside target, idle -> PASS", decideVerdict({ worstMs: 20, targetMs: 300, contended: false }), "PASS");
check("inside target, busy -> PASS", decideVerdict({ worstMs: 20, targetMs: 300, contended: true }), "PASS");
check(
  "outside target, idle -> FAIL (a real regression)",
  decideVerdict({ worstMs: 1352, targetMs: 300, contended: false }),
  "FAIL",
);
check(
  "outside target, busy -> INCONCLUSIVE (the case that caused a false failure)",
  decideVerdict({ worstMs: 1352, targetMs: 300, contended: true }),
  "INCONCLUSIVE",
);
// The boundary: the target is `< 300`, so exactly 300 is outside it. Stated because an off-by-one here
// would silently widen the requirement.
check(
  "exactly at the target is outside it",
  decideVerdict({ worstMs: 300, targetMs: 300, contended: false }),
  "FAIL",
);

console.log("  contention detection");

// `os.loadavg()` reports zeros on Windows, so both branches are real and both need to work.
check(
  "a load above the core count is contended",
  isContended({ loadavg: [16, 8, 4], cores: 8, cpuSeconds: 0, wallMs: 1 }).contended,
  true,
);
check(
  "a load below the core count is not",
  isContended({ loadavg: [2, 2, 2], cores: 8, cpuSeconds: 0, wallMs: 1 }).contended,
  false,
);
check(
  "the fallback flags more than a core's worth of CPU per second",
  isContended({ loadavg: [0, 0, 0], cores: 8, cpuSeconds: 72, wallMs: 1000 }).contended,
  true,
);
check(
  "the fallback does not flag an idle machine",
  // 8 cores, 4 seconds of CPU consumed in 1 second of wall time: half the machine. The units are the
  // point of this case — an earlier version of `isContended` divided by 1000 as well and made 72
  // core-seconds per second look like 0.072, which is the difference between "overloaded" and "idle".
  isContended({ loadavg: [0, 0, 0], cores: 8, cpuSeconds: 4, wallMs: 1000 }).contended,
  false,
);
check(
  "zero cores cannot divide by zero",
  isContended({ loadavg: [0, 0, 0], cores: 0, cpuSeconds: 0, wallMs: 1000 }).contended,
  false,
);

if (failures) {
  console.log(`  ${failures} failure(s)`);
  process.exit(1);
}
console.log("  all latency-verdict cases hold");
