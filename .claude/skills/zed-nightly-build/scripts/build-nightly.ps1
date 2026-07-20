# Builds the Zed Nightly Windows installer from the local checkout.
#
# What it does:
#   1. Temporarily sets crates/zed/RELEASE_CHANNEL to "nightly" (restored on exit,
#      even if the build fails).
#   2. Runs script/bundle-windows.ps1 in a separate pwsh process with the CI env
#      var removed, so the Azure signing gates stay off.
#   3. Copies the resulting installer to C:\Users\Public\Downloads\Zed-Nightly-Setup.exe
#      so every Windows user on this machine can run it.
#   4. With -Install, silently updates the current user's Zed Nightly right away.
#
# Requirements: PowerShell 7 (auto-relaunches itself), Inno Setup 6, VS Build
# Tools + Windows SDK, cargo. The build takes 40-90 minutes.

[CmdletBinding()]
param(
    [string]$Architecture = "x86_64",
    [switch]$Install,
    [switch]$SkipCopy,
    [switch]$DryRun
)

$ErrorActionPreference = "Stop"

# bundle-windows.ps1 uses PowerShell 7 syntax (ternary operator), so Windows
# PowerShell 5.1 cannot run it. Relaunch ourselves under pwsh transparently.
if ($PSVersionTable.PSVersion.Major -lt 7) {
    $pwsh = Get-Command pwsh -ErrorAction SilentlyContinue
    if (-not $pwsh) {
        throw "PowerShell 7 (pwsh) is required. Install it with: winget install -e --id Microsoft.PowerShell"
    }
    $forward = @("-NoProfile", "-File", $PSCommandPath, "-Architecture", $Architecture)
    if ($Install) { $forward += "-Install" }
    if ($SkipCopy) { $forward += "-SkipCopy" }
    if ($DryRun) { $forward += "-DryRun" }
    & $pwsh.Source @forward
    exit $LASTEXITCODE
}

$repoRoot = (& git -C $PSScriptRoot rev-parse --show-toplevel).Trim() -replace "/", "\"
$channelFile = Join-Path $repoRoot "crates\zed\RELEASE_CHANNEL"
$bundleScript = Join-Path $repoRoot "script\bundle-windows.ps1"
$setupPath = Join-Path $repoRoot "target\Zed-$Architecture.exe"
$publicCopy = "C:\Users\Public\Downloads\Zed-Nightly-Setup.exe"

# --- Preflight checks -------------------------------------------------------
foreach ($tool in @("cargo", "git")) {
    if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) {
        throw "'$tool' is not on PATH."
    }
}
$iscc = @(
    "C:\Program Files (x86)\Inno Setup 6\ISCC.exe",
    "C:\Program Files\Inno Setup 6\ISCC.exe",
    "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe"
) | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $iscc) {
    throw "Inno Setup 6 not found. Install it with: winget install -e --id JRSoftware.InnoSetup"
}
if (-not (Test-Path $bundleScript)) {
    throw "Not a Zed checkout: $bundleScript is missing."
}

$headSha = (& git -C $repoRoot rev-parse HEAD).Trim()
$originalChannel = [System.IO.File]::ReadAllText($channelFile)

Write-Host "Repo:         $repoRoot"
Write-Host "HEAD:         $headSha"
Write-Host "Channel:      $($originalChannel.Trim()) -> nightly (temporary)"
Write-Host "Inno Setup:   $iscc"
Write-Host "Architecture: $Architecture"

# The installed build stamps this sha; the auto-updater later compares it with
# the sha in the latest GitHub release tag. Warn when they will never match.
$unpushed = & git -C $repoRoot log --oneline "origin/main..HEAD" 2>$null
if ($unpushed) {
    Write-Warning "HEAD has commits that are not on origin/main. A CI release built from origin/main will have a different sha, so auto-update will replace this local build as soon as such a release appears."
}

# --- Build ------------------------------------------------------------------
[System.IO.File]::WriteAllText($channelFile, "nightly")
try {
    if ($DryRun) {
        Write-Host "[dry-run] Would run: pwsh -File $bundleScript -Architecture $Architecture (with CI env removed)"
        Write-Host "[dry-run] Would produce: $setupPath"
        if (-not $SkipCopy) { Write-Host "[dry-run] Would copy to: $publicCopy" }
        if ($Install) { Write-Host "[dry-run] Would install silently for the current user" }
    }
    else {
        Push-Location $repoRoot
        try {
            # Separate pwsh process: keeps bundle's `exit` from killing us and
            # gives a trustworthy exit code. CI is removed so the signing gates
            # in bundle-windows.ps1 and zed.iss stay off.
            & pwsh -NoProfile -Command "Remove-Item Env:CI -ErrorAction SilentlyContinue; Set-Location '$repoRoot'; & '$bundleScript' -Architecture $Architecture; exit `$LASTEXITCODE"
            if ($LASTEXITCODE -ne 0) { throw "bundle-windows.ps1 failed with exit code $LASTEXITCODE" }
        }
        finally {
            Pop-Location
        }
    }
}
finally {
    [System.IO.File]::WriteAllText($channelFile, $originalChannel)
    Write-Host "Channel restored to '$($originalChannel.Trim())'."
}

if ($DryRun) {
    Write-Host "[dry-run] OK - all checks passed."
    exit 0
}

# --- Publish locally --------------------------------------------------------
if (-not (Test-Path $setupPath)) {
    throw "Build reported success but $setupPath does not exist."
}
$installer = Get-Item $setupPath
Write-Host ("Installer: {0} ({1:N1} MB, built {2})" -f $installer.FullName, ($installer.Length / 1MB), $installer.LastWriteTime)

if (-not $SkipCopy) {
    Copy-Item $setupPath $publicCopy -Force
    Write-Host "Copied to $publicCopy - any Windows user on this machine can run it to update their Zed Nightly (settings and data are kept; they live in each user's %LOCALAPPDATA%\ZedNightly)."
}

if ($Install) {
    Write-Host "Installing silently for the current user..."
    Start-Process -FilePath $setupPath -ArgumentList "/VERYSILENT", "/NORESTART" -Wait
    Write-Host "Installed. A running Zed Nightly keeps the old version until it is restarted."
}

Write-Host "Done. Built from $headSha."
