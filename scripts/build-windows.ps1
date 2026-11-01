# One-shot Windows build: installs JS deps, fetches ffmpeg sidecars, builds MSI + NSIS installers.
# Prerequisites (once): Rust (rustup, MSVC toolchain), Node.js 20+, Visual Studio Build Tools (C++).
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
if (-not (Test-Path "src-tauri\binaries\ffmpeg-*.exe")) { & "$PSScriptRoot\fetch-ffmpeg.ps1" }
npm ci
npx tauri build
Write-Host ""
Write-Host "Installers:"
Get-ChildItem "target\release\bundle" -Recurse -Include *.msi, *.exe | ForEach-Object { Write-Host "  $($_.FullName)" }
