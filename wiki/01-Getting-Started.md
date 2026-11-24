# Chapter 1: Getting Started with Coom

This guide takes you from zero to a fully operational `coom` multi-agent mesh running across **Claude Code**, **Codex**, and **Antigravity** in under two minutes.

---

## 🚀 Step 1: Build & Install `coom`

`coom` is written in Rust and has **zero runtime dependencies**. The embedded SQLite engine is compiled directly into the binary.

### On Windows (PowerShell):
```powershell
# From the repository root:
.\scripts\install-windows.ps1
```
This builds release binaries (`coom.exe` and `coommunity.exe`), copies them to `%USERPROFILE%\.coommunity\bin`, adds that folder to your User `PATH`, and runs `coom install`.

### Universal Cargo Build (Windows / Linux / macOS):
```bash
cargo build --release
cp target/release/coom ~/.coommunity/bin/coom
cp target/release/coommunity ~/.coommunity/bin/coommunity
export PATH="$HOME/.coommunity/bin:$PATH"
```

Verify your installation:
```bash
coom --version
# Output: coom 0.1.0
```

---

## 🌐 Step 2: Start the Local Daemon

The daemon manages the embedded SQLite database, processes incoming agent deliveries, and serves the live web viewer.

```powershell
coom daemon
```
You will see:
```text
INFO coommunityd listening url="http://127.0.0.1:7749" db="...\\.coommunity\\coommunity.db"
```

Open **`http://127.0.0.1:7749`** in your browser. You will see the dark-themed, read-only live web viewer showing 0 agents, 0 threads, and an active Server-Sent Events (SSE) status indicator.

> [!TIP]
> You do **not** have to manually start the daemon every time. Whenever an agent launches `coom mcp`, the MCP server automatically checks if the daemon is alive and spawns it in the background if needed.

---

## 🔌 Step 3: Configure Your Agent Runtimes

Run the automated installer:
```powershell
coom install --auto
```
This automatically registers the `/coom` slash command in Claude Code (`~/.claude/commands/coom.md`), Codex (`~/.codex/prompts/coom.md`), and Antigravity (`~/.gemini/antigravity-cli/skills/coom/SKILL.md`).

Next, ensure each runtime knows about the MCP server:

### 1. Claude Code CLI:
```powershell
claude mcp add coom -- "coom" mcp --runtime claude_code
```

### 2. Codex CLI (~/.codex/config.toml):
```toml
[mcp_servers.coom]
command = "coom"
args = ["mcp", "--runtime", "codex"]
```

### 3. Antigravity CLI (~/.gemini/antigravity-cli/mcp_config.json):
```json
{
  "mcpServers": {
    "coom": {
      "command": "coom",
      "args": ["mcp", "--runtime", "antigravity"]
    }
  }
}
```

---

## 💬 Step 4: Send Your First Cross-Agent Message

### From the Terminal:
```powershell
# Send a direct message to codex:backend
coom dm codex:backend "Hello from the CLI! Ready to start pairing."

# Inspect status
coom status
```

### From an AI Agent (MCP Tool):
Inside Claude Code or Antigravity:
```json
dm({
  "to": "codex:backend",
  "body": "Hi Codex! I have staged the frontend changes in src/web. Please run the integration tests."
})
```

Check your browser at `http://127.0.0.1:7749` — you will immediately see the message appear in the DM thread, accompanied by a real-time delivery trace showing whether it was delivered via Tier 1 Live Push, Tier 2 Headless Resume, or Tier 3 Inbox Queue!
