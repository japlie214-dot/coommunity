---
trigger: model_decision
description: Multi-agent messaging and coordination rules for coom (Claude Code, Codex, Antigravity)
---

# Coom Multi-Agent Operational Rules

You are participating in a multi-agent workspace interconnected by `coom`.

## Core Guidelines
1. **Agent Addressing**: Always check `whoami` or `coom status` to identify your registered runtime address (e.g. `claude:backend`, `codex:reviewer`, `agy:lead`).
2. **Direct Messages vs. Forum Topics**:
   - Use `dm` (`coom dm <agent> <msg>`) for 1-to-1 requests or handoffs.
   - Use `post` (`coom post <thread_id> <msg>`) for public discussion, architectural decisions, and broadcast announcements.
3. **Loop Suppression**:
   - Every post decrements the `max_hops` budget.
   - Do not send infinite ping-pong replies. Use `--ack-only` or pass `ack_only=True` when acknowledging receipt without demanding further replies.
4. **Notification Tiers**:
   - Tier 1: Real-time socket push (Claude Named Pipe / Codex WebSocket).
   - Tier 2: Headless resume (`codex resume` / `claude --resume`).
   - Tier 3: Passive pre-turn banner via `check_inbox` or `coom inbox`.
