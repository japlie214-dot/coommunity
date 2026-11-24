# Install coom binary and Antigravity plugin
$ErrorActionPreference = "Stop"

Write-Host "[coom] Building release binaries..." -ForegroundColor Cyan
cargo build --release --offline

# 1. Install binaries to User Profile .coommunity\bin
$BinDir = Join-Path $env:USERPROFILE ".coommunity\bin"
New-Item -ItemType Directory -Force -Path $BinDir | Out-Null
$DestCoommunity = Join-Path $BinDir "coommunity.exe"
$DestCoom = Join-Path $BinDir "coom.exe"
Copy-Item ".\target\release\coommunity.exe" $DestCoommunity -Force
Copy-Item ".\target\release\coom.exe" $DestCoom -Force

$UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
if ($UserPath -notlike "*$BinDir*") {
    [Environment]::SetEnvironmentVariable("Path", "$UserPath;$BinDir", "User")
    Write-Host "[coom] Added $BinDir to User PATH." -ForegroundColor Yellow
}

# 2. Install to Antigravity global plugin directory
$AgyPluginDir = "C:\Users\10102721\.gemini\config\plugins\coom"
$AgySkillsDir = Join-Path $AgyPluginDir "skills\coom"
New-Item -ItemType Directory -Force -Path $AgySkillsDir | Out-Null

Copy-Item ".\plugins\coom\plugin.json" (Join-Path $AgyPluginDir "plugin.json") -Force
Copy-Item ".\plugins\coom\mcp_config.json" (Join-Path $AgyPluginDir "mcp_config.json") -Force
Copy-Item ".\plugins\coom\skills\coom\SKILL.md" (Join-Path $AgySkillsDir "SKILL.md") -Force
Write-Host "[coom] Installed global Antigravity plugin to $AgyPluginDir" -ForegroundColor Green

# 3. Also install workspace .agents skill
$WsSkillDir = ".\.agents\skills\coom"
New-Item -ItemType Directory -Force -Path $WsSkillDir | Out-Null
Copy-Item ".\plugins\coom\skills\coom\SKILL.md" (Join-Path $WsSkillDir "SKILL.md") -Force
Write-Host "[coom] Installed workspace skill to $WsSkillDir" -ForegroundColor Green

# 4. Run automated slash-command setup
& $DestCoom install --auto

# 5. Start daemon detached and run live verification test
Write-Host "[coom] Running live verification..." -ForegroundColor Cyan
$DaemonProc = Start-Process -FilePath $DestCoom -ArgumentList "daemon" -PassThru -WindowStyle Hidden
Start-Sleep -Milliseconds 800

Write-Host "[coom] Checking status:" -ForegroundColor Cyan
& $DestCoom status

Write-Host "[coom] Testing CLI message dispatch:" -ForegroundColor Cyan
& $DestCoom dm agy:test "Self-verification test from installer" --from cli:installer

Write-Host "[coom] Checking inbox for agy:test:" -ForegroundColor Cyan
& $DestCoom inbox --agent agy:test

Write-Host "[coom] Installation and live verification successful!" -ForegroundColor Green
