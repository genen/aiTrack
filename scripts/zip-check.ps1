# Shared by msi-zip.ps1 and portable-zip.ps1: list a zip's files and fail if a required one is missing.
function Assert-ZipHas {
    param([string]$Zip, [string[]]$Required)
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $archive = [System.IO.Compression.ZipFile]::OpenRead($Zip)
    try {
        $names = @($archive.Entries | ForEach-Object { $_.FullName.Replace('\', '/') })
        Write-Output "Contents of $(Split-Path $Zip -Leaf):"
        foreach ($e in $archive.Entries) { Write-Output ("  {0,10:N0}  {1}" -f $e.Length, $e.FullName.Replace('\', '/')) }
        foreach ($r in $Required) {
            if ($names -notcontains $r) { throw "$r is missing from $Zip." }
        }
        $pdf = $archive.Entries | Where-Object { $_.FullName.Replace('\', '/') -eq 'README.pdf' } | Select-Object -First 1
        if ($pdf -and $pdf.Length -lt 20000) { throw "README.pdf in $Zip is only $($pdf.Length) bytes." }
    } finally {
        $archive.Dispose()
    }
}

# Files that go next to the program in every zip: the licence text, the third-party notices, the README (markdown, with its
# screenshots in docs\img so the pictures show) and the README as a PDF made by readme-pdf.ps1.
function Copy-ReadmeFiles {
    param([string]$Root, [string]$OutDir, [string]$Stage)
    $pdf = Join-Path $OutDir 'README.pdf'
    foreach ($f in @((Join-Path $Root 'LICENSE'), (Join-Path $Root 'THIRD-PARTY-NOTICES.md'), (Join-Path $Root 'README.md'), $pdf)) {
        if (-not (Test-Path $f)) { throw "$f was not found. Run scripts\readme-pdf.ps1 first (it makes README.pdf)." }
        Copy-Item $f $Stage
    }
    $img = Join-Path $Root 'docs\img'
    if (-not (Test-Path $img)) { throw "$img was not found." }
    New-Item -ItemType Directory (Join-Path $Stage 'docs') | Out-Null
    Copy-Item $img (Join-Path $Stage 'docs\img') -Recurse
}
