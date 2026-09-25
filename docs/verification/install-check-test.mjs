#!/usr/bin/env node
// The platform gate for the Windows-install check, asserted in both directions.
//
// # Why the direction matters more than the gate
//
// The bug this fixes was a check running where it should not: on a Linux CI runner, `install.ps1` correctly
// refused with `unsupported architecture:`, and the check called that a failure.
//
// The fix is one condition, and the dangerous way to get it wrong is **inverted** — skipping on Windows and
// running on Linux, which is exactly the broken state. **This machine is Windows, so an inverted gate would
// look perfectly healthy here** and fail only in CI. That is the same shape as every other bug this session
// has recorded: a defect invisible from where you are standing.
//
// So both directions are asserted, not just the one that applies locally.
import { windowsInstallCheckVerdict } from "./install-check.mjs";

let failures = 0;
function check(name, actual, expected) {
  if (actual === expected) {
    console.log(`  ok    ${name}`);
  } else {
    console.log(`  FAIL  ${name} — expected ${expected}, got ${actual}`);
    failures += 1;
  }
}

console.log("  Windows install check: where it may run");

// The case that broke CI. This assertion is the one that would have caught it.
check(
  "Linux with PowerShell must SKIP, not run and die",
  windowsInstallCheckVerdict({ platform: "linux", hasPowershell: true, hasScript: true }),
  "skip-not-windows",
);
check(
  "macOS with PowerShell must SKIP",
  windowsInstallCheckVerdict({ platform: "darwin", hasPowershell: true, hasScript: true }),
  "skip-not-windows",
);
// The inverse, which an inverted gate would get wrong and no local run would reveal.
check(
  "Windows with PowerShell must RUN",
  windowsInstallCheckVerdict({ platform: "win32", hasPowershell: true, hasScript: true }),
  "run",
);
check(
  "Windows without PowerShell must SKIP",
  windowsInstallCheckVerdict({ platform: "win32", hasPowershell: false, hasScript: true }),
  "skip-no-powershell",
);
check(
  "a missing script must SKIP",
  windowsInstallCheckVerdict({ platform: "win32", hasPowershell: true, hasScript: false }),
  "skip-missing-script",
);

if (failures) {
  console.log(`  ${failures} failure(s)`);
  process.exit(1);
}
console.log("  the platform gate holds in both directions");
