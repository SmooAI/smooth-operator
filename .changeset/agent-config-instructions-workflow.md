---
"@smooai/smooth-operator": minor
---

Per-agent behavior config: honor `instructions` + run `conversation_workflow` (SMOODEV-590).

The reference server resolved a turn's system prompt from **per-org** settings, so every agent in an org spoke with the same voice and `conversation_workflow` was never applied — a public chat agent ignored its own persona and behaved as the generic customer-support bot.

Config-delivery seam (matches the sibling Python/TS/C#/Go lanes): `AgentConfigResolver::resolve(agent_id)` — the ws protocol's `create_conversation_session` carries only an agent UUID, so config is resolved **server-side by id**. Default `StaticAgentConfigResolver` (empty ⇒ no-op, behavior unchanged); a `PgAgentConfigResolver` reads the monorepo `agents` table on the adapter's existing pool. The runner now:

- uses the agent's `instructions` (+ `personality.persona`) as the system prompt, overriding the org default;
- injects the agent's `greeting` into the prompt only on the first turn of a conversation;
- restricts the turn's tools to `tool_config.enabledTools` (`enabled == true` entries by snake_case `toolId`; empty/absent ⇒ full set; unknown ids ignored), and delivers each entry's `config` to the tool via `ToolProviderContext`;
- enforces per-tool `authLevel` at execution against the agent's `visibility` (a `ToolHook` gate: admin blocked on public agents; internal auto-satisfies; end_user on public requires an identity-verified session, fail-closed — the OTP flow is a host seam);
- when a `conversation_workflow` is set, injects the current step's intent/criteria and, after each turn, runs a cheap failure-tolerant judge on the configurable `judge_model` (haiku-tier default) to advance the step; the step id is tracked per session.

Per-agent isolation, malformed-jsonb tolerance (degrade to org default, never crash the turn), judge-failure tolerance (stay on the current step), and the authLevel branches (admin/end_user/internal, authed vs not) are covered by unit + integration tests.
