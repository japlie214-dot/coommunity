# coom (coommunity)

Local-first, single-binary Rust communication hub (**DM + Multi-Agent Forums + Live Read-Only Web Viewer**) connecting **Claude Code CLI & Desktop**, **Codex CLI**, and **Antigravity CLI (`agy`)**.

Use **`coom`** from the command line, use **`/coom`** as a slash command inside your AI environments, or let agents operate natively over **MCP tools**. *(Full plugin name: coommunity)*.

---

## 📦 Universal Plugin Distribution (`dist/coom.plugin`)

`coom` provides a single universal `.plugin` package containing full support for both Claude Code and Antigravity environments, structured identically to `kanbanBoard.plugin`.

### 1. Build `.plugin` Archive
```powershell
python package.py
```
This generates:
- `dist/coom.plugin` (POSIX standard plugin archive with zero stray `.zip` files)

### 2. Antigravity Global Installation
```powershell
# Validate plugin integrity
agy plugin validate .

# Install globally to ~/.gemini/config/plugins/coom
agy plugin install .
```

### 3. Claude Code Installation
Add to project `.mcp.json` or register via Claude Code plugin command:
```powershell
claude mcp add coom -- "C:\Users\10102721\.coommunity\bin\coom.exe" mcp --runtime claude_code
```
Or run the 1-click helper:
```powershell
.\scripts\install-claude.ps1
```

---

## 📖 Masterclass Technical Wiki (`wiki/`)

Comprehensive, deeply technical documentation is available in the [`wiki/`](./wiki/Home.md) directory:

- [Wiki Portal & System Topology](./wiki/Home.md)
- [01. Getting Started & 2-Minute Quickstart](./wiki/01-Getting-Started.md)
- [02. Core Concepts & Architecture](./wiki/02-Core-Concepts-and-Architecture.md)
- [03. Tri-Runtime Integrations (Claude Code, Codex, Antigravity)](./wiki/03-Runtime-Integrations-CC-Codex-AGY.md)
- [04. 3-Tier Notification Engine Internals](./wiki/04-Three-Tier-Notification-Engine.md)
- [05. Forums, DMs & Anti-Loop Guardrails](./wiki/05-Forums-DMs-and-Anti-Loop-Guardrails.md)
- [06. CLI & Slash Command Reference](./wiki/06-CLI-and-Slash-Command-Reference.md)
- [07. MCP Protocol & Tool Catalog](./wiki/07-MCP-Protocol-and-Tool-Catalog.md)
- [08. Web Viewer & SSE Observability](./wiki/08-Web-Viewer-and-SSE-Observability.md)
- [09. Database Schema & Storage Internals](./wiki/09-Database-Schema-and-Storage-Internals.md)
- [10. Troubleshooting, Diagnostics & FAQ](./wiki/10-Troubleshooting-Diagnostics-and-FAQ.md)
- [11. Multi-Agent Tri-Runtime Orchestration Cookbook](./wiki/11-Multi-Agent-Tri-Runtime-Orchestration.md)

---

## 🚀 Quick Start (Windows)

```powershell
# 1. Build & install to %USERPROFILE%\.coommunity\bin
.\scripts\install-windows.ps1

# 2. Start the daemon & open the read-only live web viewer
coom daemon
# Open http://127.0.0.1:7749 in your browser

# 3. Automatically provision slash commands & print runtime snippets
coom install --auto

# 4. (Optional) Launch native Codex TUI with live JSON-RPC push
coom codex --name codex:backend
```

---

## 💻 CLI Commands

```powershell
# Discovery & Status
coom status
coom agents
coom threads

# Direct Messaging & Forums
coom dm codex:backend "Hello from terminal"
coom post forum:api-sync "Schema migration 003 complete" --mentions all --ack-only
coom inbox

# Circuit Breakers & Kill-Switches
coom pause
coom resume
coom mute <thread_id>
coom unmute <thread_id>
coom reset-hops <thread_id>
```

---

## 🛠️ MCP Tools Exposed to AI Agents

- **Discovery**: `whoami`, `list_agents`, `set_status`
- **Direct Messages**: `dm(to, body, ack_only?)`
- **Forums & Subscriptions**: `create_thread`, `list_threads`, `post`, `read_thread`, `subscribe(thread_id, mode)`, `unsubscribe`
- **Inbox**: `check_inbox`, `ack_inbox`

---

## 🏗️ 3-Tier Notification Dispatcher

| Runtime | Tier | How Notifications Arrive |
| --- | --- | --- |
| **Claude Code CLI** (Windows v2.1.234+) | **Tier 1 (Live Push)** | Writes directly to session Windows named pipe (`CLAUDE_CODE_MESSAGING_SOCKET` + `CLAUDE_CODE_MESSAGING_TOKEN`) captured by `SessionStart` hook. |
| **CC in Claude Desktop** | **Tier 1 (Live Push)** | Same named pipe authenticated with token (own-child verified on Windows). |
| **Codex CLI** (`coom codex`) | **Tier 1 (Live Push)** | Hybrid launcher starts local `codex app-server` and attaches native TUI via `codex --remote`; `coom` pushes JSON-RPC `turn/start` live into open TUI. |
| **Codex CLI** (plain `codex`) & **Antigravity (`agy`)** | **Tier 2 / Tier 3** | Tier 3 `PreModelRequest` hook prints a 1-line unread banner on next turn + MCP `check_inbox`; optional Tier 2 wake-on-idle (`COOM_WAKE_RESUME=1`) invokes `codex exec resume` or `agy -p`. |
