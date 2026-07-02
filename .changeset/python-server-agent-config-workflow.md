---
"@smooai/smooth-operator": minor
---

Python server: honor per-agent config + implement conversation workflows (SMOODEV-590).

Agents served by the Python server previously ignored their per-agent config and always used the generic server-wide "customer support agent" persona. Now:

- **Per-agent `instructions`** drive the system prompt for that agent's conversations, overriding the server-wide default (falling back to it when unset). Per-agent `personality` and first-turn `greeting` are plumbed into the prompt; `tool_config` is carried through.
- **`conversation_workflow`** is implemented as a stepped, judge-advanced guided flow: the current step's intent + criteria are rendered into the system prompt, and a cheap post-turn judge call decides whether the criteria were met and advances to the next step (explicit `next` → sequential → terminal). The current step id is tracked per conversation.

Config parsing is tolerant — a malformed workflow or config degrades to the server default and never crashes a session. The judge is failure-tolerant — any judge error leaves the conversation on the current step. Delivery seam: `ServerState.agent_config_resolver` (`AgentConfigResolver.resolve(agentId)`, default dict-backed `StaticAgentConfigResolver`) is resolved per turn from the session's agent — the ws protocol carries only an agent UUID, so config is looked up server-side. Empty resolver → behavior unchanged. Mirrors the Rust reference PR.
