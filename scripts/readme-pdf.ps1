<#
.SYNOPSIS
  Makes README.pdf from README.md (same commit, so it cannot go stale). Output: dist\README.pdf

.DESCRIPTION
  PowerShell 7's ConvertFrom-Markdown turns README.md into HTML; the screenshots in docs\img are inlined as data
  URIs and a print stylesheet is added; Microsoft Edge (installed on Windows runners) prints it to PDF headless.
  Build-time tools only: nothing of Edge or PowerShell goes into the zips. Fails if the PDF is missing, empty,
  does not start with %PDF, or is under 20 KB.
#>
param(
    [string]$OutDir = ''
)
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
if (-not $OutDir) { $OutDir = Join-Path $root 'dist' }
New-Item -ItemType Directory -Force $OutDir | Out-Null
$OutDir = (Resolve-Path $OutDir).Path
$pdf = Join-Path $OutDir 'README.pdf'
if (Test-Path $pdf) { Remove-Item $pdf -Force }

$watch = [System.Diagnostics.Stopwatch]::StartNew()
$html = (ConvertFrom-Markdown -Path (Join-Path $root 'README.md')).Html

# inline every local image so the PDF does not depend on the folder it was made in
$html = [regex]::Replace($html, 'src="(docs/img/[^"]+)"', {
    param($m)
    $file = Join-Path $root ($m.Groups[1].Value -replace '/', '\')
    if (-not (Test-Path $file)) { throw "The README shows $($m.Groups[1].Value), which does not exist." }
    $b64 = [Convert]::ToBase64String([System.IO.File]::ReadAllBytes($file))
    'src="data:image/png;base64,' + $b64 + '"'
})
if ($html -match 'src="docs/') { throw 'A local image was not inlined.' }

$css = @'
@page { size: A4; margin: 16mm 15mm; }
body { font-family: "Segoe UI", Arial, sans-serif; font-size: 10.5pt; line-height: 1.45; color: #1a1a1a; }
h1 { font-size: 22pt; border-bottom: 2px solid #ccc; padding-bottom: 4pt; }
h2 { font-size: 15pt; border-bottom: 1px solid #ddd; padding-bottom: 3pt; margin-top: 18pt; page-break-after: avoid; }
h3, h4 { page-break-after: avoid; }
pre { background: #f4f4f4; border: 1px solid #ddd; padding: 6pt 8pt; white-space: pre-wrap; word-wrap: break-word; page-break-inside: avoid; }
code { font-family: Consolas, "Courier New", monospace; font-size: 9.5pt; background: #f4f4f4; }
pre code { background: none; }
table { border-collapse: collapse; margin: 6pt 0; page-break-inside: avoid; }
th, td { border: 1px solid #bbb; padding: 3pt 6pt; text-align: left; vertical-align: top; }
th { background: #eee; }
blockquote { border-left: 3px solid #bbb; margin-left: 0; padding-left: 10pt; color: #444; }
img { max-width: 100%; height: auto; page-break-inside: avoid; }
a { color: #0b5ca8; text-decoration: none; }
'@
$page = "<!doctype html><html><head><meta charset=`"utf-8`"><title>aiTrack README</title><style>$css</style></head><body>$html</body></html>"
$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("aitrack-readme-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory $tmp | Out-Null
try {
    $htmlFile = Join-Path $tmp 'README.html'
    [System.IO.File]::WriteAllText($htmlFile, $page, (New-Object System.Text.UTF8Encoding($false)))

    $edge = @(
        "${env:ProgramFiles(x86)}\Microsoft\Edge\Application\msedge.exe",
        "$env:ProgramFiles\Microsoft\Edge\Application\msedge.exe",
        "$env:ProgramFiles\Google\Chrome\Application\chrome.exe"
    ) | Where-Object { $_ -and (Test-Path $_) } | Select-Object -First 1
    if (-not $edge) { throw 'Neither Microsoft Edge nor Chrome was found to print the PDF.' }
    Write-Output "Printing with $edge"
    $uri = ([System.Uri]$htmlFile).AbsoluteUri
    $args = @('--headless=new', '--disable-gpu', '--no-sandbox', '--no-pdf-header-footer',
        "--user-data-dir=$(Join-Path $tmp 'profile')", "--print-to-pdf=$pdf", $uri)
    $p = Start-Process -FilePath $edge -ArgumentList $args -PassThru -NoNewWindow
    if (-not $p.WaitForExit(120000)) { $p.Kill($true); Write-Output 'The browser did not exit in 120 s; it was stopped.' }
} finally {
    if (Test-Path $tmp) { Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue }
}

if (-not (Test-Path $pdf)) { throw "README.pdf was not written." }
$bytes = [System.IO.File]::ReadAllBytes($pdf)
if ($bytes.Length -lt 20000) { throw "README.pdf is only $($bytes.Length) bytes." }
if ([System.Text.Encoding]::ASCII.GetString($bytes, 0, 5) -ne '%PDF-') { throw 'README.pdf does not start with %PDF-.' }
Write-Output ("Built {0} ({1:N0} bytes) in {2:N1} s" -f $pdf, $bytes.Length, $watch.Elapsed.TotalSeconds)
