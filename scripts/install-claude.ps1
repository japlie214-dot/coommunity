# Install coom integration for Claude Code & Claude Desktop
$ErrorActionPreference = "Stop"

$BinDir = Join-Path $env:USERPROFILE ".coommunity\bin"
$CoomExe = Join-Path $BinDir "coom.exe"

if (-not (Test-Path $CoomExe)) {
    Write-Warning "coom.exe not found at $CoomExe. Building release binary..."
    cargo build --release --offline
    New-Item -ItemType Directory -Force -Path $BinDir | Out-Null
    Copy-Item ".\target\release\coom.exe" $CoomExe -Force
}

# 1. Install Claude slash command (~/.claude/commands/coom.md)
$ClaudeUserDir = Join-Path $env:USERPROFILE ".claude"
$ClaudeCommandsDir = Join-Path $ClaudeUserDir "commands"
New-Item -ItemType Directory -Force -Path $ClaudeCommandsDir | Out-Null
Copy-Item ".\plugins\coom\claude\commands\coom.md" (Join-Path $ClaudeCommandsDir "coom.md") -Force
Write-Host "[Claude] Installed slash command to $ClaudeCommandsDir\coom.md" -ForegroundColor Green

# Also ensure workspace .claude/commands/coom.md exists
$LocalClaudeDir = ".\.claude\commands"
New-Item -ItemType Directory -Force -Path $LocalClaudeDir | Out-Null
Copy-Item ".\plugins\coom\claude\commands\coom.md" (Join-Path $LocalClaudeDir "coom.md") -Force
Write-Host "[Claude] Installed project slash command to $LocalClaudeDir\coom.md" -ForegroundColor Green

# 2. Register MCP server in Claude Code if CLI is installed
if (Get-Command "claude" -ErrorAction SilentlyContinue) {
    Write-Host "[Claude] Registering MCP server via 'claude mcp add'..." -ForegroundColor Cyan
    claude mcp add coom -- "$CoomExe" mcp --runtime claude_code
} else {
    Write-Host "[Claude] 'claude' CLI not in PATH. You can register MCP server with:" -ForegroundColor Yellow
    Write-Host "  claude mcp add coom -- `"$CoomExe`" mcp --runtime claude_code" -ForegroundColor White
}

# 3. Configure Claude Desktop if present (%APPDATA%\Claude\claude_desktop_config.json)
$ClaudeDesktopConfigPath = Join-Path $env:APPDATA "Claude\claude_desktop_config.json"
$ClaudeDesktopDir = Split-Path $ClaudeDesktopConfigPath
if (Test-Path $ClaudeDesktopDir) {
    Write-Host "[Claude Desktop] Found Claude Desktop directory, configuring MCP..." -ForegroundColor Cyan
    $cfg = @{ mcpServers = @{} }
    if (Test-Path $ClaudeDesktopConfigPath) {
        try {
            $existing = Get-Content $ClaudeDesktopConfigPath -Raw | ConvertFrom-Json
            if ($existing.mcpServers) {
                $cfg.mcpServers = $existing.mcpServers
            }
        } catch {}
    }
    $cfg.mcpServers["coom"] = @{
        command = $CoomExe
        args = @("mcp", "--runtime", "claude_desktop")
    }
    $cfg | ConvertTo-Json -Depth 10 | Set-Content $ClaudeDesktopConfigPath -Encoding UTF8
    Write-Host "[Claude Desktop] Updated $ClaudeDesktopConfigPath" -ForegroundColor Green
}

Write-Host "`n[Claude] Setup complete! Start a new Claude session and type /coom or use coom MCP tools." -ForegroundColor Green
