<#
.SYNOPSIS
  Builds everything a release has: the release exe, the MSI, the MSI in a zip and the portable zip, in .\dist.

.DESCRIPTION
  Needs Rust (1.89 or newer) and the .NET SDK (8 or newer).
    .\scripts\build-release.ps1
#>
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
Push-Location $root
try {
    cargo build --release
    if ($LASTEXITCODE -ne 0) { throw "cargo build --release failed (exit code $LASTEXITCODE)." }
    & (Join-Path $PSScriptRoot 'installer.ps1')
    & (Join-Path $PSScriptRoot 'readme-pdf.ps1')
    & (Join-Path $PSScriptRoot 'msi-zip.ps1')
    & (Join-Path $PSScriptRoot 'portable-zip.ps1')
} finally {
    Pop-Location
}
