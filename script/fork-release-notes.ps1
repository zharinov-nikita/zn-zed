<#
.SYNOPSIS
Writes release notes for a zn-zed nightly build.

.DESCRIPTION
Diffs the commit being built against the previous nightly release and renders
the result as markdown. Lives in a file rather than inline in
.github/workflows/fork-release.yml so the preflight job can parse it before a
two-hour build starts.

Needs GH_TOKEN in the environment for `gh release list`, and a checkout with
full history (fetch-depth: 0) so the previous nightly's commit is present.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string] $Sha,
    [Parameter(Mandatory)][string] $OutFile
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
# `git cat-file -e` returns 1 by design when the object is missing; don't let
# that turn into a terminating error on pwsh versions where native exit codes
# honor $ErrorActionPreference.
$PSNativeCommandUseErrorActionPreference = $false

# The release for $Sha doesn't exist yet, so the latest one is the previous
# nightly. Its tag is `nightly-<version>-<40-hex-sha>`.
$prevTag = gh release list --limit 1 --json tagName -q '.[0].tagName' 2>$null
$prevSha = if ($prevTag) { ($prevTag -split '-')[-1] } else { $null }

$prevIsInHistory = $false
if ($prevSha) {
    git cat-file -e "$prevSha^{commit}" 2>$null
    $prevIsInHistory = ($LASTEXITCODE -eq 0)
    if (-not $prevIsInHistory) {
        Write-Host "Previous nightly $prevSha is not in this checkout; skipping changelog."
    }
}

$body = "Automated build of $Sha"
if ($prevIsInHistory) {
    # --first-parent keeps the log to commits that landed directly on main:
    # your own feat/fix commits, plus each upstream merge collapsed to a single
    # line instead of thousands of upstream commits.
    $log = git log --first-parent --pretty=format:'- %s (%h)' "$prevSha..$Sha"
    if ($log) {
        $range = "$($prevSha.Substring(0, 7))..$($Sha.Substring(0, 7))"
        $body = "### Changes since previous nightly ($range)`n`n" + ($log -join "`n")
    } else {
        $body = "No changes since previous nightly ($($prevSha.Substring(0, 7)))."
    }
}

$dir = Split-Path -Parent $OutFile
if ($dir -and -not (Test-Path $dir)) {
    New-Item -ItemType Directory -Path $dir -Force | Out-Null
}
Set-Content -Path $OutFile -Value $body -NoNewline
Write-Host "Wrote release notes to ${OutFile}:"
Write-Host $body
