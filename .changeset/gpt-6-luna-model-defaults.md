---
"@smooai/smooth-operator": minor
---

SMOODEV-3342: every server now defaults its main turn to `gpt-6-luna` and its conversation-workflow judge to `groq-gpt-oss-120b`.

**Behaviour change — defaults only.** A deployment that sets `SMOOTH_AGENT_MODEL` / `SMOOTH_AGENT_JUDGE_MODEL` (or an org / agent / per-turn model) is unaffected. An unconfigured server used to request `claude-haiku-4-5` for both; on the Smoo AI gateway those calls were failing over to whatever LiteLLM's fallback chain picked. The Smoo AI gateway's policy is now that every default LLM call uses the gpt-6-luna family or a Groq alias.

- **Main-turn default `claude-haiku-4-5` → `gpt-6-luna`** in the Rust server + Lambda + `dev-support` example, and the Go, Python, .NET and TypeScript servers (plus the Helm values, Argo CD example, SST example and `examples/.env.example`, which shipped `gemini-2.5-flash`). The Go and Python servers now pass the default to the engine explicitly instead of leaving the model empty, so the engine's own built-in fallback model is never what goes on the wire.
- **The judge has its own default, `groq-gpt-oss-120b`** (`DEFAULT_JUDGE_MODEL` in Rust, `DefaultJudgeModel` in Go, `WORKFLOW_JUDGE_MODEL` in Python, `ServerEnv.DefaultJudgeModel` in .NET, `DEFAULT_JUDGE_MODEL` in TS). It no longer follows the main-turn model. `SMOOTH_AGENT_JUDGE_MODEL` still overrides it (the .NET host now also reads that canonical name, keeping `SMOOTH_JUDGE_MODEL` as an alias).
- **Judge output cap raised to 512 tokens in every port** (Rust was 16, the others 200). The new default judge is a reasoning model, and reasoning tokens count against `max_tokens`: a small cap was spent entirely on reasoning, the reply came back empty, and the workflow never advanced. The prompt still asks for a one-word JSON verdict.
- Evals: `CHEAP_MODEL` (the default agent + judge model) is now `gpt-6-luna`; the nightly matrix grades `groq-gpt-oss-120b` and `gpt-6-luna`, both judged by `gpt-6-luna`. Live E2E tests run on `gpt-6-luna`.
- `GET /admin/settings` for an org that never saved settings reports `gpt-6-luna` in the Go, Python, .NET and TS servers.
