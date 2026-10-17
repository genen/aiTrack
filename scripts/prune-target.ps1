<#
.SYNOPSIS
  Deletes STALE build artifacts from cargo's target directory (never sources). PowerShell twin of
  scripts/prune-target.sh.

.DESCRIPTION
  Every code change gives each test binary and example a new hash-named file
  (target\debug\deps\<name>-<hash>) and cargo never removes the old ones, so a development machine
  slowly fills up. This removes the superseded copies and nothing else:
    * in <target>\{debug,release}\{deps,examples} and <target>\<triple>\{debug,release}\{deps,examples},
      of the hash-suffixed artifacts (name-<16 hex>.ext, larger than -MinSizeMB, default 2) that share a
      name and extension, every one except the NEWEST, once it has not been modified for -Minutes
      (default 30). The newest build of each crate / test / example is never deleted, however old, so
      live dependencies are not rebuilt on every run;
    * incremental compilation directories older than 45 minutes;
    * with -AllButNewest: the same superseded builds without the -Minutes wait (only the 5 minute
      floor applies);
    * with -Emergency (disk full): ALSO every file larger than -MinSizeMB in those folders not
      modified for -Minutes, newest builds and dependencies included. The next build recompiles them.
  Nothing modified in the last 5 minutes is ever touched (a build may be running). Reparse points
  (symlinks, junctions) are never followed. Refuses to run unless the folder has cargo's CACHEDIR.TAG.

.EXAMPLE
  .\scripts\prune-target.ps1 -DryRun
  .\scripts\prune-target.ps1 -Minutes 15 -AllButNewest
#>
[CmdletBinding()]
param(
    [string]$Target = (Join-Path $PSScriptRoot '..\target'),
    [int]$Minutes = 30,
    [int]$MinSizeMB = 2,
    [switch]$AllButNewest,
    [switch]$Emergency,
    [switch]$DryRun
)

$ErrorActionPreference = 'Stop'
$SafeMinutes = 5
$IncrementalMinutes = 45

if (-not (Test-Path -LiteralPath $Target -PathType Container)) {
    Write-Host "prune-target: $Target is not a directory (nothing to do)"
    exit 0
}
$item = Get-Item -LiteralPath $Target -Force
if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) {
    Write-Host "prune-target: $Target is a link (nothing to do)"
    exit 0
}
$Target = $item.FullName
$tag = Join-Path $Target 'CACHEDIR.TAG'
if (-not ((Test-Path -LiteralPath $tag) -and ((Get-Content -LiteralPath $tag -Raw) -match 'Signature: 8a477f597d28d172789f06886806bc55'))) {
    Write-Error "prune-target: $Target has no cargo CACHEDIR.TAG, refusing to delete anything"
    exit 1
}

function Test-Link($p) { [bool]((Get-Item -LiteralPath $p -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) }

$profileDirs = @()
foreach ($c in @('debug', 'release')) {
    $profileDirs += Join-Path $Target $c
    foreach ($triple in Get-ChildItem -LiteralPath $Target -Directory -Force | Where-Object { -not (Test-Link $_.FullName) }) {
        $profileDirs += Join-Path $triple.FullName $c
    }
}
$profileDirs = $profileDirs | Where-Object { (Test-Path -LiteralPath $_ -PathType Container) -and -not (Test-Link $_) } | Select-Object -Unique

$script:bytes = 0
$script:count = 0
function Remove-Stale($path, $size, $why) {
    $rel = $path.Substring($Target.Length).TrimStart('\', '/')
    $mb = [math]::Floor($size / 1MB)
    if ($DryRun) { Write-Host "would remove $rel ($mb MB, $why)" }
    else {
        try { Remove-Item -LiteralPath $path -Recurse -Force -ErrorAction Stop; Write-Host "removed $rel ($mb MB, $why)" }
        catch { return }
    }
    $script:bytes += $size
    $script:count += 1
}

$now = Get-Date
$age = [math]::Max($Minutes, $SafeMinutes)
foreach ($pd in $profileDirs) {
    foreach ($sub in 'deps', 'examples') {
        $dir = Join-Path $pd $sub
        if (-not (Test-Path -LiteralPath $dir -PathType Container) -or (Test-Link $dir)) { continue }
        $files = Get-ChildItem -LiteralPath $dir -File -Force |
            Where-Object { -not ($_.Attributes -band [IO.FileAttributes]::ReparsePoint) -and $_.Length -gt ($MinSizeMB * 1MB) }
        $gone = @{}
        if ($Emergency) {
            foreach ($f in $files | Where-Object { ($now - $_.LastWriteTime).TotalMinutes -gt $age }) {
                Remove-Stale $f.FullName $f.Length "emergency: not modified for $age min"
                $gone[$f.FullName] = $true
            }
        }
        # superseded builds: group name-<16 hex>.ext by name+ext, newest first; all but the first go
        # once old enough. Files that are not hash-suffixed are not judged and stay.
        $wait = if ($AllButNewest) { $SafeMinutes } else { $age }
        $groups = $files | Where-Object { $_.Name -match '-[0-9a-f]{16}(\.[^.]*)?$' } |
            Group-Object { ($_.Name -replace '-[0-9a-f]{16}', '') }
        foreach ($g in $groups) {
            foreach ($old in $g.Group | Sort-Object LastWriteTime -Descending | Select-Object -Skip 1) {
                if ($gone.ContainsKey($old.FullName)) { continue }
                if (($now - $old.LastWriteTime).TotalMinutes -gt $wait) {
                    Remove-Stale $old.FullName $old.Length "older build of $($g.Name)"
                }
            }
        }
    }
    $inc = Join-Path $pd 'incremental'
    if ((Test-Path -LiteralPath $inc -PathType Container) -and -not (Test-Link $inc)) {
        foreach ($d in Get-ChildItem -LiteralPath $inc -Directory -Force |
                Where-Object { -not ($_.Attributes -band [IO.FileAttributes]::ReparsePoint) -and (($now - $_.LastWriteTime).TotalMinutes -gt $IncrementalMinutes) }) {
            $size = (Get-ChildItem -LiteralPath $d.FullName -Recurse -File -Force -ErrorAction SilentlyContinue | Measure-Object Length -Sum).Sum
            Remove-Stale $d.FullName ([int64]$size) "incremental data older than $IncrementalMinutes min"
        }
    }
}
$verb = if ($DryRun) { 'Would reclaim' } else { 'Reclaimed' }
Write-Host ("{0} {1} MB in {2} item(s) from {3}" -f $verb, [math]::Floor($script:bytes / 1MB), $script:count, $Target)
