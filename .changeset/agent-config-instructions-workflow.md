---
"@smooai/smooth-operator": minor
---

Per-agent behavior config: honor `instructions` + run `conversation_workflow`.

The reference server resolved a turn's system prompt from **per-org** settings, so every agent in an org spoke with the same voice and `conversation_workflow` was never applied — a public chat agent ignored its own persona and behaved as the generic customer-support bot.

Adds a per-**agent** seam (`AgentConfigProvider`, defaulting to a no-op; a Postgres impl reads the monorepo `agents` table on the adapter's existing pool). The runner now:

- uses the agent's `instructions` (+ `personality.persona`, `greeting`) as the system prompt, overriding the org default;
- when a `conversation_workflow` is set, injects the current step's intent/criteria into the prompt and, after each turn, runs a cheap failure-tolerant judge (yes/no/maybe) to advance the step; the current step id is tracked on the session.

Per-agent isolation, malformed-jsonb tolerance (degrade to org default, never crash the turn), and judge-failure tolerance (stay on the current step) are covered by tests. Mirrors the TS reference (SMOODEV-590).
