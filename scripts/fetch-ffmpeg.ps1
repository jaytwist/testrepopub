# Downloads ffmpeg + ffprobe for Windows and places them where Tauri expects sidecar binaries:
#   src-tauri/binaries/ffmpeg-x86_64-pc-windows-msvc.exe
#   src-tauri/binaries/ffprobe-x86_64-pc-windows-msvc.exe
#
# Default: BtbN "LGPL" build (no GPL encoders such as libx264; the app falls back to Windows'
# Media Foundation H.264 encoder or VP9 for video previews). Use -Gpl for the GPL build if the
# app stays for personal use.
param([switch]$Gpl)

$ErrorActionPreference = "Stop"
$flavor = if ($Gpl) { "gpl" } else { "lgpl" }
$name = "ffmpeg-master-latest-win64-$flavor"
$url = "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/$name.zip"
$root = Split-Path -Parent $PSScriptRoot
$bin = Join-Path $root "src-tauri\binaries"
$tmp = Join-Path $env:TEMP "mo-ffmpeg"

New-Item -ItemType Directory -Force -Path $bin, $tmp | Out-Null
$zip = Join-Path $tmp "$name.zip"
Write-Host "Downloading $url"
Invoke-WebRequest -Uri $url -OutFile $zip
Expand-Archive -Path $zip -DestinationPath $tmp -Force
$triple = (rustc -vV | Select-String "host:").ToString().Split(" ")[1].Trim()
foreach ($tool in "ffmpeg", "ffprobe") {
    Copy-Item (Join-Path $tmp "$name\bin\$tool.exe") (Join-Path $bin "$tool-$triple.exe") -Force
}
Write-Host "OK: ffmpeg/ffprobe for $triple in $bin"
