# One-shot Windows build: installs JS deps, fetches ffmpeg sidecars, builds MSI + NSIS installers.
# Prerequisites (once): Rust (rustup, MSVC toolchain), Node.js 20+, Visual Studio Build Tools (C++).
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
if (-not (Test-Path "src-tauri\binaries\ffmpeg-*.exe")) { & "$PSScriptRoot\fetch-ffmpeg.ps1" }
# People feature runtime + models (verified by SHA-256; skipped quickly when already present).
& "$PSScriptRoot\fetch-ai.ps1"
npm ci
# Auto-update files are signed with the updater key (kept out of git in .secrets\).
if (-not $env:TAURI_SIGNING_PRIVATE_KEY) {
    $key = Join-Path $root ".secrets\updater.key"
    if (Test-Path $key) {
        $env:TAURI_SIGNING_PRIVATE_KEY = Get-Content -Raw $key
    } else {
        Write-Host "업데이트 서명 키가 없어 자동 업데이트 파일 없이 빌드합니다."
        $env:TAURI_CONFIG = '{"bundle":{"createUpdaterArtifacts":false}}'
    }
}
npx tauri build
Write-Host ""
Write-Host "Installers:"
Get-ChildItem "target\release\bundle" -Recurse -Include *.msi, *.exe | ForEach-Object { Write-Host "  $($_.FullName)" }
