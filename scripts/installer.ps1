<#
.SYNOPSIS
  Builds the Windows installer (MSI) from target\release\aitrack.exe.

.DESCRIPTION
  Run `cargo build --release` first. Needs the .NET SDK (8 or newer): the WiX Toolset v6 is a NuGet package
  that dotnet restores by itself, nothing else has to be installed.

  Output: dist\aiTrack-Setup-<version>-win-x64.msi, for example aiTrack-Setup-0.1.4-beta-win-x64.msi.
  The MSI installs for all users (Program Files, administrator prompt).
#>
param(
    [string]$Exe = '',
    [string]$OutDir = ''
)
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
. (Join-Path $PSScriptRoot 'release-info.ps1')
$release = Get-AiTrackRelease -Root $root

if (-not $Exe) { $Exe = Join-Path $root 'target\release\aitrack.exe' }
if (-not $OutDir) { $OutDir = Join-Path $root 'dist' }
if (-not (Test-Path $Exe)) {
    throw "aitrack.exe was not found at $Exe. Run 'cargo build --release' in the repository folder first."
}
$dotnet = Get-Command dotnet -ErrorAction SilentlyContinue
if (-not $dotnet) {
    throw 'The .NET SDK was not found (dotnet is not on PATH). Install it from https://dotnet.microsoft.com/download (version 8 or newer), open a new PowerShell and try again.'
}

# The product's UpgradeCode never changes: it is how a newer setup finds and replaces an older install.
$upgradeCode = '53A66D9A-D10F-4538-911D-605495D66BAE'

$files = @{
    Icon    = Join-Path $root 'assets\icon\aitrack.ico'
    Rtf     = Join-Path $root 'installer\License.rtf'
    License = Join-Path $root 'LICENSE'
    Readme  = Join-Path $root 'README.md'
    Notices = Join-Path $root 'THIRD-PARTY-NOTICES.md'
    Project = Join-Path $root 'installer\aiTrack.Installer.wixproj'
}
foreach ($name in $files.Keys) {
    if (-not (Test-Path $files[$name])) { throw "$name was not found at $($files[$name])." }
}
# The licence page is generated from LICENSE (python installer\make-license-rtf.py); make sure it is the GPL text.
if (([System.IO.File]::ReadAllText($files.Rtf)) -notlike '*GNU GENERAL PUBLIC LICENSE*') {
    throw 'installer\License.rtf does not contain the GPL text. Run: python installer\make-license-rtf.py'
}

New-Item -ItemType Directory -Force $OutDir | Out-Null
$OutDir = (Resolve-Path $OutDir).Path
# MSBuild properties: forward slashes keep the paths simple inside the define list
$fwd = { param($p) (Resolve-Path $p).Path -replace '\\', '/' }
foreach ($stale in @((Join-Path $root 'installer\obj'), (Join-Path $root 'installer\bin'))) {
    if (Test-Path $stale) { Remove-Item $stale -Recurse -Force }
}
$env:DOTNET_CLI_TELEMETRY_OPTOUT = '1'
Write-Output "Building the installer for aiTrack $($release.Version) (MSI version $($release.Msi))..."
dotnet build $files.Project -c Release -t:Rebuild `
    "-p:InstallerPlatform=x64" "-p:ProductVersion=$($release.Msi)" "-p:UpgradeCode=$upgradeCode" `
    "-p:ExePath=$(& $fwd $Exe)" "-p:IconPath=$(& $fwd $files.Icon)" "-p:LicenseRtf=$(& $fwd $files.Rtf)" `
    "-p:LicenseTxt=$(& $fwd $files.License)" "-p:ReadmePath=$(& $fwd $files.Readme)" "-p:NoticesPath=$(& $fwd $files.Notices)" `
    -o $OutDir
if ($LASTEXITCODE -ne 0) { throw "The installer build failed (dotnet build exit code $LASTEXITCODE). The WiX messages are above." }

$built = Join-Path $OutDir 'aiTrack.msi'
$named = Join-Path $OutDir "aiTrack-Setup-$($release.Version)-win-x64.msi"
if (-not (Test-Path $built)) { throw "WiX did not write $built." }
Move-Item $built $named -Force
Get-ChildItem $OutDir -File | Where-Object { $_.Extension -in '.cab', '.pdb', '.wixpdb' } | Remove-Item -Force
if ((Get-Item $named).Length -lt 100KB) { throw "$named is suspiciously small." }
$hash = (Get-FileHash $named -Algorithm SHA256).Hash.ToLowerInvariant()
Write-Output "Built $named"
Write-Output "SHA256 $hash"
