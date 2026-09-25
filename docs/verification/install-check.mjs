// What the Windows-install check should do on a given host, as a pure function.
//
// # Why this is a module with its own test
//
// The check ran on a **Linux CI runner**, where PowerShell is installed but `$env:PROCESSOR_ARCHITECTURE` is
// not, so `install.ps1` died with `unsupported architecture:` — the installer correctly refusing a platform
// it cannot serve — and the check reported that as a **failure**. Two CI runs went red over a host the check
// was never meant for.
//
// The mistake was asking "did the command exit zero" without asking "was this command meant to run here".
// The fix is a platform gate, and a platform gate has an obvious failure mode of its own: **inverted**, so
// it skips on Windows and runs on Linux — which is precisely the state that broke. That is not hypothetical;
// it is what `!onWindows` written as `onWindows` would do, and **no run on this machine would notice,
// because this machine is Windows.**
//
// So the decision is a function and `install-check.mjs` asserts every case. The precedent is the same as
// `measure.mjs`: a decision inside a large check is a decision nothing can test.
export function windowsInstallCheckVerdict({ platform, hasPowershell, hasScript }) {
  if (!hasScript) return "skip-missing-script";
  if (!hasPowershell) return "skip-no-powershell";
  // `install.ps1` refuses any host whose `PROCESSOR_ARCHITECTURE` is not `AMD64`, so on Linux and macOS the
  // correct permanent outcome is a skip rather than a run that dies.
  if (platform !== "win32") return "skip-not-windows";
  return "run";
}
