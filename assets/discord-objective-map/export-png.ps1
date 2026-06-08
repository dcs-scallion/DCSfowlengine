# Export SVG markers to 96x96 PNG for Mapbox Static API (@2x maps).
# Requires Inkscape on PATH: https://inkscape.org/
$ErrorActionPreference = "Stop"
$root = $PSScriptRoot
$inkscape = Get-Command inkscape -ErrorAction SilentlyContinue
if (-not $inkscape) {
    Write-Error "Inkscape not found on PATH. Install Inkscape or export PNG manually from svg/."
}
New-Item -ItemType Directory -Force -Path (Join-Path $root "png\96") | Out-Null
Get-ChildItem (Join-Path $root "svg\*.svg") | ForEach-Object {
    $out = Join-Path $root "png\96\$($_.BaseName).png"
    & inkscape $_.FullName --export-type=png --export-filename $out -w 96 -h 96
    Write-Host "Wrote $out"
}
