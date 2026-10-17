<#
.SYNOPSIS
  Reads the product version from Cargo.toml (dot-source this file, then call Get-AiTrackRelease).

.DESCRIPTION
  Cargo.toml has the one version, for example 0.1.4-beta.
    Version   the full Cargo version                     0.1.4-beta   (file names, release tag v0.1.4-beta)
    Msi       numeric MSI ProductVersion, four parts     0.1.4.0      (the pre-release suffix is dropped)
    Channel   beta when the version has a -beta suffix, else stable
    Tag       v<Version>
#>
function Get-AiTrackRelease {
    param([string]$Root = (Split-Path $PSScriptRoot -Parent))
    $cargo = Join-Path $Root 'Cargo.toml'
    if (-not (Test-Path $cargo)) { throw "Cargo.toml was not found at $cargo." }
    $text = [System.IO.File]::ReadAllText($cargo)
    # the package version is the first line that starts with `version =`
    if ($text -notmatch '(?m)^version\s*=\s*"(\d+)\.(\d+)\.(\d+)(?:-([0-9A-Za-z.-]+))?"') {
        throw 'No version = "X.Y.Z" or "X.Y.Z-suffix" line was found in Cargo.toml.'
    }
    $core = "$($Matches[1]).$($Matches[2]).$($Matches[3])"
    $suffix = $Matches[4]
    $version = if ($suffix) { "$core-$suffix" } else { $core }
    # the numeric MSI version has to be below 256.256.65536 (build) and only four parts at most
    foreach ($part in @($Matches[1], $Matches[2])) { if ([int]$part -gt 255) { throw "Version part $part is too large for an MSI version." } }
    if ([int]$Matches[3] -gt 65535) { throw "Version part $($Matches[3]) is too large for an MSI version." }
    $channel = if ($suffix -match '(^|[.-])beta($|[.-])') { 'beta' } else { 'stable' }
    [pscustomobject]@{
        Version = $version
        Core    = $core
        Msi     = "$core.0"
        Channel = $channel
        Tag     = "v$version"
    }
}
