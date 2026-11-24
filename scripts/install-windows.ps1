# Build and install coom.exe and coommunity.exe on Windows
$ErrorActionPreference = "Stop"

Write-Host "[coom] Building release binaries..." -ForegroundColor Cyan
cargo build --release

$BinDir = Join-Path $env:USERPROFILE ".coommunity\bin"
New-Item -ItemType Directory -Force -Path $BinDir | Out-Null
$DestCoommunity = Join-Path $BinDir "coommunity.exe"
$DestCoom = Join-Path $BinDir "coom.exe"

# Gracefully terminate running instances if file is locked
Get-Process -Name "coom", "coommunity" -ErrorAction SilentlyContinue | ForEach-Object {
    Write-Host "[coom] Stopping running process $($_.Name) (PID $($_.Id)) to allow clean binary replacement..." -ForegroundColor Yellow
    Stop-Process -Id $_.Id -Force -ErrorAction SilentlyContinue
}
Start-Sleep -Milliseconds 500

Copy-Item ".\target\release\coommunity.exe" $DestCoommunity -Force
Copy-Item ".\target\release\coom.exe" $DestCoom -Force

$UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
if ($UserPath -notlike "*$BinDir*") {
    [Environment]::SetEnvironmentVariable("Path", "$UserPath;$BinDir", "User")
    Write-Host "[coom] Added $BinDir to User PATH (restart terminal to pick up)." -ForegroundColor Yellow
}

Write-Host "[coom] Installed to $DestCoom and $DestCoommunity" -ForegroundColor Green
& $DestCoom install
