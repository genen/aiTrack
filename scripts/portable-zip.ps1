<#
.SYNOPSIS
  Makes the portable zip: aitrack.exe, LICENSE, README.md (with docs\img) and README.pdf. No installer, no administrator rights.

.DESCRIPTION
  Run `cargo build --release` and scripts\readme-pdf.ps1 first. Output: dist\aiTrack-<version>-win-x64.zip
#>
param(
    [string]$Exe = '',
    [string]$OutDir = ''
)
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
. (Join-Path $PSScriptRoot 'release-info.ps1')
. (Join-Path $PSScriptRoot 'zip-check.ps1')
$release = Get-AiTrackRelease -Root $root
if (-not $Exe) { $Exe = Join-Path $root 'target\release\aitrack.exe' }
if (-not $OutDir) { $OutDir = Join-Path $root 'dist' }
if (-not (Test-Path $Exe)) { throw "aitrack.exe was not found at $Exe. Run 'cargo build --release' first." }
foreach ($f in 'LICENSE', 'README.md') {
    if (-not (Test-Path (Join-Path $root $f))) { throw "$f was not found in $root." }
}
New-Item -ItemType Directory -Force $OutDir | Out-Null
$zip = Join-Path (Resolve-Path $OutDir).Path "aiTrack-$($release.Version)-win-x64.zip"
$stage = Join-Path ([System.IO.Path]::GetTempPath()) ("aitrack-zip-" + [guid]::NewGuid().ToString('N'))
try {
    New-Item -ItemType Directory $stage | Out-Null
    Copy-Item $Exe (Join-Path $stage 'aitrack.exe')
    Copy-ReadmeFiles -Root $root -OutDir $OutDir -Stage $stage
    if (Test-Path $zip) { Remove-Item $zip -Force }
    Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $zip
} finally {
    if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
}
if (-not (Test-Path $zip)) { throw "The zip was not written: $zip" }
Assert-ZipHas -Zip $zip -Required @('aitrack.exe', 'LICENSE', 'README.md', 'README.pdf', 'docs/img/home-dark.png')
Write-Output "Built $zip"
Write-Output "SHA256 $((Get-FileHash $zip -Algorithm SHA256).Hash.ToLowerInvariant())"
