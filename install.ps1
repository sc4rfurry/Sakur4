<#
.SYNOPSIS
    Install sakur4d on Windows.

.DESCRIPTION
    The counterpart to `install.sh`, which handles Linux and macOS. That script refuses Windows by
    design — its `uname` case accepts only `Linux` and `Darwin` — and this project's README advertised
    Windows in its platform badge while the only automated path was a shell script Windows users cannot
    run. The release has published `sakur4d-<version>-x86_64-pc-windows-msvc.zip` since v0.1.0, so the
    artifact was always there and only the installation was missing.

    What it does, in the same order and with the same refusals as the shell installer:

      1. Download the archive for this platform from the latest release (or `-Version`).
      2. Download `SHA256SUMS.txt` — and **refuse to install without it**, rather than continuing
         unverified. A missing checksum file is a failure, not a silent downgrade.
      3. Compare checksums, and refuse on a mismatch.
      4. Extract, install the binary, and place the Agent Skill.
      5. Say where things went, and what is not on PATH.

.PARAMETER Version
    A release tag, e.g. `v0.1.0`. Default: the latest release.

.PARAMETER BinDir
    Where to put `sakur4d.exe`. Default: `%USERPROFILE%\.sakur4\bin`, which is created if needed.

.PARAMETER SkillDir
    Where to place the Agent Skill. Default: `%USERPROFILE%\.agents\skills`.

.PARAMETER Force
    Replace an existing skill. Without it, an existing skill is left alone — replacing one a user has
    edited is worse than saying it is already there.

.PARAMETER DryRun
    Do everything except install: resolve the release, download, verify the checksum, and report what
    would be written. Useful for checking a release without touching the machine.

.EXAMPLE
    irm https://raw.githubusercontent.com/sc4rfurry/Sakur4/master/install.ps1 | iex

.EXAMPLE
    .\install.ps1 -Version v0.1.0 -DryRun
#>
[CmdletBinding()]
param(
    [string]$Version,
    [string]$BinDir,
    [string]$SkillDir,
    [switch]$Force,
    [switch]$DryRun
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

# The repository, not an environment default. `install.sh` read `sakur4/sakur4` — an organisation
# nobody owns — for a while, and every install path went to a 404. One source of truth, here.
$Repo = if ($env:SAKUR4_REPO) { $env:SAKUR4_REPO } else { 'sc4rfurry/Sakur4' }
$Bin = 'sakur4d'

if (-not $Version) { $Version = $env:SAKUR4_VERSION }
if (-not $BinDir) { $BinDir = $env:SAKUR4_BIN_DIR }
if (-not $SkillDir) { $SkillDir = $env:SAKUR4_SKILL_DIR }
if ($env:SAKUR4_FORCE) { $Force = $true }

function Say([string]$Message) { Write-Host $Message }
function Die([string]$Message) { Write-Error "install: $Message"; exit 1 }

# --- Which platform ------------------------------------------------------------------------------
#
# The release publishes `x86_64-pc-windows-msvc` and nothing else for Windows, so an ARM64 host would
# get an x64 binary that runs under emulation. Saying so is better than a wrong download: Windows on
# ARM does run x64 binaries, and a user who wants a native build needs to know they are not getting one.
$arch = $env:PROCESSOR_ARCHITECTURE
if ($arch -eq 'ARM64') {
    Say "  note       Windows on ARM: installing the x64 build, which runs under emulation"
} elseif ($arch -ne 'AMD64') {
    Die "unsupported architecture: $arch. The release publishes x86_64 only; see https://github.com/$Repo/releases"
}
$target = 'x86_64-pc-windows-msvc'

# --- Which version --------------------------------------------------------------------------------
if (-not $Version) {
    Say "  resolving  the latest release"
    try {
        $latest = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest" `
            -Headers @{ 'User-Agent' = 'sakur4-installer' }
        $Version = $latest.tag_name
    } catch {
        Die "could not determine the latest release ($($_.Exception.Message)); pass -Version"
    }
}
if (-not $Version) { Die 'could not determine the latest release; pass -Version' }

$archive = "$Bin-$Version-$target.zip"
$base = "https://github.com/$Repo/releases/download/$Version"

# --- Download -------------------------------------------------------------------------------------
#
# A file, not a pipe. `irm | iex` is convenient for the script itself, but an installer that streams an
# archive into a shell teaches people to trust the output of a pipe — and the checksum below needs a
# file to hash anyway.
$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("sakur4-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tmp -Force | Out-Null

Say '  sakur4d installer'
Say "  version    $Version"
Say "  target     $target"
Say "  fetching   $archive"

try {
    Invoke-WebRequest -Uri "$base/$archive" -OutFile (Join-Path $tmp $archive) `
        -Headers @{ 'User-Agent' = 'sakur4-installer' }
} catch {
    Die "download failed: $base/$archive ($($_.Exception.Message))"
}

$size = (Get-Item (Join-Path $tmp $archive)).Length
Say "  size       $size bytes"

try {
    Invoke-WebRequest -Uri "$base/SHA256SUMS.txt" -OutFile (Join-Path $tmp 'SHA256SUMS.txt') `
        -Headers @{ 'User-Agent' = 'sakur4-installer' }
} catch {
    Die "could not fetch SHA256SUMS.txt; refusing to install without verification"
}

# --- Verify ---------------------------------------------------------------------------------------
#
# Both checksum-file formats: `sha256sum ./*.zip` writes `hash *./name`, a bare name gives `hash  name`.
# `install.sh` matched only the second at first, and every install would have been refused with "not
# listed in SHA256SUMS.txt" — which reads as a corrupt download rather than an installer bug.
$expected = $null
foreach ($line in Get-Content (Join-Path $tmp 'SHA256SUMS.txt')) {
    $parts = $line -split '\s+', 2
    if ($parts.Count -lt 2) { continue }
    $name = $parts[1].Trim()
    $name = $name -replace '^\*', ''
    $name = $name -replace '^\./', ''
    if ($name -eq $archive) { $expected = $parts[0].Trim().ToLowerInvariant(); break }
}
if (-not $expected) { Die "$archive is not listed in SHA256SUMS.txt; refusing to install" }

$actual = (Get-FileHash -Path (Join-Path $tmp $archive) -Algorithm SHA256).Hash.ToLowerInvariant()
if ($expected -ne $actual) {
    Die "checksum mismatch for $archive`n  expected $expected`n  actual   $actual`nDo not use this download. Report it at https://github.com/$Repo/issues"
}
Say '  checksum   ok'

# --- Install --------------------------------------------------------------------------------------
Expand-Archive -Path (Join-Path $tmp $archive) -DestinationPath $tmp -Force
$root = Join-Path $tmp "$Bin-$Version-$target"
if (-not (Test-Path $root)) { Die "the archive did not contain $Bin-$Version-$target" }

if (-not $BinDir) {
    $BinDir = Join-Path $env:USERPROFILE '.sakur4\bin'
}
if ($DryRun) {
    Say "  would put  $(Join-Path $BinDir "$Bin.exe")"
} else {
    New-Item -ItemType Directory -Path $BinDir -Force | Out-Null
    # Move to a temporary name, then rename: a half-written binary on PATH is worse than none.
    $staging = Join-Path $BinDir "$Bin.exe.new"
    Copy-Item (Join-Path $root "$Bin.exe") $staging -Force
    Move-Item $staging (Join-Path $BinDir "$Bin.exe") -Force
    Say "  installed  $(Join-Path $BinDir "$Bin.exe")"
}

# --- The Agent Skill ------------------------------------------------------------------------------
if (-not $SkillDir) { $SkillDir = Join-Path $env:USERPROFILE '.agents\skills' }
$skillSource = Join-Path $root 'skills\sakur4'
if (Test-Path $skillSource) {
    $skillTarget = Join-Path $SkillDir 'sakur4'
    if ((Test-Path $skillTarget) -and -not $Force) {
        Say "  skill      already at $skillTarget (use -Force to replace)"
    } elseif ($DryRun) {
        Say "  would put  the skill at $skillTarget"
    } else {
        New-Item -ItemType Directory -Path $SkillDir -Force | Out-Null
        Copy-Item $skillSource $skillTarget -Recurse -Force
        Say "  skill      $skillTarget"
    }
}

# --- PATH -----------------------------------------------------------------------------------------
$onPath = ($env:PATH -split ';') -contains $BinDir
if (-not $onPath -and -not $DryRun) {
    $userPath = [Environment]::GetEnvironmentVariable('PATH', 'User')
    [Environment]::SetEnvironmentVariable('PATH', "$userPath;$BinDir", 'User')
    Say "  PATH       added $BinDir to your user PATH"
    Say '             open a new terminal for it to take effect'
} elseif ($onPath) {
    Say "  PATH       $BinDir is already on PATH"
}

Say ''
Say "  next       $Bin config claude      # configuration for your harness"

if (-not $DryRun) {
    # The archive is deleted on the way out, so any instruction that pointed inside it would be a lie —
    # which is exactly what the shell installer did with its skill path for several releases.
    Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
    Say ''
    Say "  The archive is extracted and removed; nothing remains to copy from it."
    Say "  The OMP and Hermes plugins ship inside it, and each harness keeps plugins elsewhere:"
    Say "  re-download the archive if you want them, or see https://github.com/$Repo/wiki/Harnesses."
}
