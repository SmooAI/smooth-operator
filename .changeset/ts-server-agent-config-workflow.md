---
"@smooai/smooth-operator-server": minor
---

TS server: honor per-agent config + implement conversation workflows (SMOODEV-590).

Agents served by the TypeScript operator now respect their own per-agent config instead of all sharing one generic org persona. A new `AgentConfigResolver` seam resolves a session's `agentId` into its `AgentConfig` (instructions, `conversationWorkflow`, greeting, personality, tool allow-list); the resolver is server-side because the `create_conversation_session` payload carries only an `agentId`. An un-configured agent (no resolver, or resolver returns undefined) falls back to the server/org default prompt + full tool set, so existing behavior is unchanged.

`conversationWorkflow` is implemented as a stepped, judge-advanced guided-agency flow: the current step's intent + criteria are rendered into the system prompt, and after each turn a cheap failure-tolerant judge LLM call decides whether the criteria were met and advances the pointer (following `next` or array order), tracked as `currentStepId` on the session. Malformed config degrades to the default flow and never crashes a session. Mirrors the Rust server's `agent-config-instructions-workflow` design.
