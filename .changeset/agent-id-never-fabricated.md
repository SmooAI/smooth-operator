---
'@smooai/smooth-operator': minor
---

Stop fabricating an `agent_id` when the caller names no agent.

Session creation filled a missing `agentId` with a fresh UUID, so every agentless session
pointed at an agent that had never existed. `Session.agent_id` is now `Option<String>` and the
`conversation_sessions.agent_id` column is nullable: absent is the honest answer, and a
fabricated reference is not.

Both entry points had the same bug — the WebSocket handler and the Lambda dispatcher — and
both are fixed. A blank or whitespace-only `agentId` now also reads as absent rather than
becoming a literal empty agent.

BREAKING for direct Rust consumers of `smooth_operator::domain::Session`: `agent_id` is now
`Option<String>`.
