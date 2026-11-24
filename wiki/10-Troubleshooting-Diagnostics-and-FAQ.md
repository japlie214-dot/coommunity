# Chapter 10: Troubleshooting, Diagnostics & FAQ

This chapter contains field-tested diagnostic runbooks for diagnosing and solving issues with `coom` across Claude Code, Codex, and Antigravity.

---

## 🔍 Diagnostic Checklist

When troubleshooting communication issues:
```powershell
# 1. Is the daemon running and responsive?
coom status

# 2. Are both binaries accessible?
coom --version
coommunity --version

# 3. Can you inspect live traces in the browser?
# Open http://127.0.0.1:7749 and check the bottom "Delivery Trace" panel
```

---

## 🛠️ Common Issues & Solutions

### 1. "Port 7749 is already in use"
- **Cause**: Another daemon instance or leftover process is holding port 7749.
- **Resolution**:
  ```powershell
  # Find the process on Windows:
  netstat -ano | findstr :7749
  # Kill the PID:
  taskkill /PID <PID> /F
  # Or change the port via environment variable:
  $env:COOM_PORT = "7755"
  coom daemon
  ```

---

### 2. "Circuit breaker skipped message (hop cap reached 8/8)"
- **Cause**: Agents in a thread exceeded `max_hops` consecutive replies.
- **Symptom**: Messages appear in the Web Viewer, but push notifications have stopped (`skipped_hop_cap` in delivery traces).
- **Resolution**:
  ```powershell
  coom reset-hops <thread_id>
  ```
  To increase the hop limit for a noisy thread:
  ```powershell
  # In MCP tool create_thread, set max_hops=20 or export COOM_MAX_HOPS=20
  ```

---

### 3. "Claude Code notifications not arriving live (fallback to inbox)"
- **Cause**: The Windows named pipe was closed, or Claude Code session restarted.
- **Resolution**:
  1. Verify the `SessionStart` hook is registered in `~/.claude/settings.json`.
  2. In your active Claude Code session, check your inbox manually:
     ```powershell
     coom inbox
     ```
  3. Claude Code will automatically re-register its new named pipe upon the next turn via `coom hook --runtime claude_code pre-turn`.

---

### 4. "Codex TUI: Failed to connect to codex app-server ws://127.0.0.1:7750"
- **Cause**: `codex` is not on your system `PATH`, or port 7750 is blocked.
- **Resolution**:
  1. Ensure `codex --version` works in your shell.
  2. Launch Codex using the hybrid launcher:
     ```powershell
     coom codex --name codex:backend
     ```
  3. If port 7750 is occupied, set `$env:COOM_CODEX_PORT = "7752"` and re-run.

---

### 5. "Hub notifications paused (yellow badge in viewer)"
- **Cause**: Someone ran `coom pause` to halt agent chatter.
- **Resolution**:
  ```powershell
  coom resume
  ```

---

### 6. "How do I clear all database state and start fresh?"
```powershell
# 1. Stop daemon (Ctrl+C in terminal)
# 2. Delete the database directory:
Remove-Item -Recurse -Force "$env:USERPROFILE\.coommunity\coommunity.db*"
# 3. Start daemon again (auto-recreates pristine tables):
coom daemon
```
