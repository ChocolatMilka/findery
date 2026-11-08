# Renders the README's pictures: tools/img/*.html -> docs/img/*.png.
# Needs a Chromium-based browser (Edge ships with Windows); set $env:CHROME to override.
# `social` is GitHub's 2:1 repository preview (1280x640); the others are cropped to their content.
$ErrorActionPreference = "Stop"
$here = $PSScriptRoot
$out = Join-Path $here "..\..\docs\img"
$chrome = $env:CHROME
if (-not $chrome) {
    $chrome = @(
        "C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
        "C:\Program Files\Google\Chrome\Application\chrome.exe"
    ) | Where-Object { Test-Path $_ } | Select-Object -First 1
}
if (-not $chrome) { throw "no Chromium-based browser found; set CHROME" }

$jobs = @(
    @{ name = "social"; w = 1280; h = 640 },
    @{ name = "layers"; w = 1600; h = 1400 },
    @{ name = "search"; w = 1600; h = 1400 }
)
foreach ($j in $jobs) {
    $png = Join-Path $out "$($j.name)-full.png"
    $url = "file:///" + ((Join-Path $here "$($j.name).html") -replace '\\', '/')
    & $chrome --headless --disable-gpu --hide-scrollbars --force-device-scale-factor=1 `
        "--window-size=$($j.w),$($j.h)" "--screenshot=$png" $url | Out-Null
    if ($j.name -eq "social") { Move-Item -Force $png ($png -replace "-full", "") }
    else { python (Join-Path $here "crop.py") $png }
}
