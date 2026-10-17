<#
.SYNOPSIS
  Puts the installer in a zip of its own, for browsers that block a bare .msi download.

.DESCRIPTION
  Run scripts\installer.ps1 first. Output: dist\aiTrack-Setup-<version>-win-x64-msi.zip, holding the MSI
  (the same bytes) beside LICENSE, README.md (with docs\img) and README.pdf (run scripts\readme-pdf.ps1 first). The zip is opened again and the MSI inside is compared with the original by SHA256.
#>
param(
    [string]$OutDir = ''
)
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
. (Join-Path $PSScriptRoot 'release-info.ps1')
. (Join-Path $PSScriptRoot 'zip-check.ps1')
$release = Get-AiTrackRelease -Root $root
if (-not $OutDir) { $OutDir = Join-Path $root 'dist' }
$OutDir = (Resolve-Path $OutDir).Path
$msiName = "aiTrack-Setup-$($release.Version)-win-x64.msi"
$msi = Join-Path $OutDir $msiName
if (-not (Test-Path $msi)) { throw "$msiName was not found in $OutDir. Run scripts\installer.ps1 first." }
$zip = Join-Path $OutDir "aiTrack-Setup-$($release.Version)-win-x64-msi.zip"
if (Test-Path $zip) { Remove-Item $zip -Force }
$stage = Join-Path ([System.IO.Path]::GetTempPath()) ("aitrack-msizip-stage-" + [guid]::NewGuid().ToString('N'))
try {
    New-Item -ItemType Directory $stage | Out-Null
    Copy-Item $msi $stage
    Copy-ReadmeFiles -Root $root -OutDir $OutDir -Stage $stage
    Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $zip
} finally {
    if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
}
if (-not (Test-Path $zip)) { throw "The zip was not written: $zip" }
Assert-ZipHas -Zip $zip -Required @($msiName, 'LICENSE', 'README.md', 'README.pdf', 'docs/img/home-dark.png')

# the MSI inside must be the same file as the bare one
$check = Join-Path ([System.IO.Path]::GetTempPath()) ("aitrack-msizip-" + [guid]::NewGuid().ToString('N'))
try {
    Expand-Archive -Path $zip -DestinationPath $check
    $inside = Join-Path $check $msiName
    if (-not (Test-Path $inside)) { throw "$msiName is not inside $zip." }
    $a = (Get-FileHash $msi -Algorithm SHA256).Hash
    $b = (Get-FileHash $inside -Algorithm SHA256).Hash
    if ($a -ne $b) { throw "The MSI inside the zip differs from the bare MSI ($b, expected $a)." }
} finally {
    if (Test-Path $check) { Remove-Item $check -Recurse -Force }
}
Write-Output "Built $zip"
Write-Output "SHA256 $((Get-FileHash $zip -Algorithm SHA256).Hash.ToLowerInvariant())"
