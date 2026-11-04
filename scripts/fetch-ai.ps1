# Downloads what the People feature needs into src-tauri\resources\ai (bundled with the installer
# as <install dir>\ai, and shipped next to the exe in the portable zip):
#   * ONNX Runtime 1.24.4 with DirectML (onnxruntime.dll, DirectML.dll) — from Microsoft's official
#     onnxruntime-directml wheel on PyPI (1.24.4 is the newest release that still has DirectML)
#   * YuNet face detector and SFace face recognizer — from OpenCV Zoo
# Every file is checked against a pinned SHA-256; a mismatch stops the build.
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$out = Join-Path $root "src-tauri\resources\ai"
$tmp = Join-Path $env:TEMP "mo-ai"
New-Item -ItemType Directory -Force -Path $out, $tmp | Out-Null

function Get-Verified([string]$Url, [string]$Dest, [string]$Sha256) {
    if ((Test-Path $Dest) -and ((Get-FileHash $Dest -Algorithm SHA256).Hash -eq $Sha256)) { return }
    Write-Host "Downloading $Url"
    Invoke-WebRequest -Uri $Url -OutFile $Dest -UseBasicParsing
    $got = (Get-FileHash $Dest -Algorithm SHA256).Hash
    if ($got -ne $Sha256) { throw "SHA-256 mismatch for $Url`n expected $Sha256`n got      $got" }
}

function Assert-Hash([string]$Path, [string]$Sha256) {
    $got = (Get-FileHash $Path -Algorithm SHA256).Hash
    if ($got -ne $Sha256) { throw "SHA-256 mismatch for $Path`n expected $Sha256`n got      $got" }
}

# Face models (OpenCV Zoo; YuNet: MIT, SFace: Apache-2.0).
$zoo = "https://media.githubusercontent.com/media/opencv/opencv_zoo/main/models"
Get-Verified "$zoo/face_detection_yunet/face_detection_yunet_2023mar.onnx" (Join-Path $out "face_detection_yunet_2023mar.onnx") `
    "8f2383e4dd3cfbb4553ea8718107fc0423210dc964f9f4280604804ed2552fa4"
Get-Verified "$zoo/face_recognition_sface/face_recognition_sface_2021dec.onnx" (Join-Path $out "face_recognition_sface_2021dec.onnx") `
    "0ba9fbfa01b5270c96627c4ef784da859931e02f04419c829e83484087c34e79"

# ONNX Runtime + DirectML (MIT; DirectML.dll under Microsoft's DirectML license, which allows
# redistribution inside applications built with ML frameworks).
$whl = Join-Path $tmp "onnxruntime_directml-1.24.4-cp312-cp312-win_amd64.zip"
Get-Verified "https://files.pythonhosted.org/packages/88/ea/33814eb0ec96775eda4c1d30b0d86e91d7d2cd0d84c66d3915aef0e06fa3/onnxruntime_directml-1.24.4-cp312-cp312-win_amd64.whl" `
    $whl "f2ecb68b7b7b259d2ef3112ae760149f9b5a1e7c0fbb73d539da6250a648a614"
$unz = Join-Path $tmp "ort"
Expand-Archive -Path $whl -DestinationPath $unz -Force
$capi = Join-Path $unz "onnxruntime\capi"
foreach ($dll in "onnxruntime.dll", "DirectML.dll", "onnxruntime_providers_shared.dll") {
    Copy-Item (Join-Path $capi $dll) (Join-Path $out $dll) -Force
}
Assert-Hash (Join-Path $out "onnxruntime.dll") "302c69f9779d63ef4ab90316e59444c4acbaca7fe3455020d79d10bcfcb00715"
Assert-Hash (Join-Path $out "DirectML.dll") "b73972115320e906a49602f2027a3266622881b0d325ba685e0f165a9482a8d7"
Copy-Item (Join-Path $unz "onnxruntime\LICENSE") (Join-Path $out "ONNXRUNTIME-LICENSE.txt") -Force
Copy-Item (Join-Path $unz "onnxruntime\ThirdPartyNotices.txt") (Join-Path $out "ONNXRUNTIME-ThirdPartyNotices.txt") -Force

Write-Host "OK: People runtime and models in $out"
