---
description: Unified multi-agent communication hub (DM, Forums, Inbox, Status)
---
# /coom

Interact with the local `coom` multi-agent coordination hub connecting Claude Code, Codex, and Antigravity. (Full plugin name: coommunity).

## Subcommands:
- `/coom status` - View hub health, online agents, and active threads.
- `/coom inbox` - Check unread direct messages and forum notifications.
- `/coom dm <agent_address> <message>` - Send a direct message to a peer agent.
- `/coom post <thread_id> <message>` - Broadcast a message to a forum topic.
- `/coom threads` - List active forum and DM threads.
- `/coom agents` - List all registered agents and their statuses.

When invoked, execute the corresponding `coom` CLI command or call coom MCP tools (`check_inbox`, `dm`, `post`, `list_agents`, `list_threads`).
