# Build a small release binary: cargo build + UPX packing.
$ErrorActionPreference = "Stop"
Set-Location $PSScriptRoot

cargo build --release
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

$exe = "target\release\nrel.exe"
$upx = (Get-Command upx -ErrorAction SilentlyContinue).Source
if (-not $upx) {
    $upx = Join-Path $env:USERPROFILE ".cache\upx\upx.exe"
    if (-not (Test-Path $upx)) {
        $dir = Split-Path $upx
        New-Item -ItemType Directory -Force -Path $dir | Out-Null
        $zip = Join-Path $dir "upx.zip"
        Invoke-WebRequest -Uri "https://github.com/upx/upx/releases/download/v4.2.4/upx-4.2.4-win64.zip" -OutFile $zip
        Expand-Archive -Path $zip -DestinationPath $dir -Force
        Copy-Item (Join-Path $dir "upx-4.2.4-win64\upx.exe") $upx -Force
    }
}

& $upx --lzma -9 $exe
Write-Host ("packed: " + (Get-Item $exe).Length + " bytes")
