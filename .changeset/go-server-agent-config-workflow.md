---
"@smooai/smooth-operator-server-go": minor
---

Go server: honor per-agent config + implement conversation workflows (SMOODEV-590).

Agents served by the Go operator now respect their own per-agent config instead of all sharing one generic org persona. A new `AgentConfigResolver` seam resolves a session's `agentId` into its `AgentConfig` (instructions, `Workflow`, greeting, personality, tool allow-list); resolution is server-side because the `create_conversation_session` payload carries only an `agentId`. An un-configured agent (no resolver, or resolver returns nil) falls back to the server/org default prompt + full tool set, so existing behavior is unchanged. Wire one in via `server.WithAgentConfigResolver`.

`conversationWorkflow` is implemented as a stepped, judge-advanced guided-agency flow: the current step's intent + criteria are rendered into the system prompt (`<ConversationWorkflow>` block), and after each turn a cheap failure-tolerant judge LLM call decides whether the criteria were met and advances the pointer (following `next` or array order), tracked as `CurrentStepID` on the session. Malformed config degrades to the default flow and never crashes a session. Mirrors the TS/Python server siblings and the Rust reference's `agent-config-instructions-workflow` design.
