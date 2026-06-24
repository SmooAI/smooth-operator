---
"@smooai/smooth-operator": minor
---

Add two host provider-injection seams to the chat runner so a deployment flavor can run a turn with its OWN tools and persona without forking the runner:

- **Custom tool injection** — a new `ToolProvider` trait (`tools_for(&ToolProviderContext) -> Vec<Arc<dyn Tool>>`) plus `AppState::with_tools(provider)`. When installed, the runner merges the provider's per-turn tools into the turn's `ToolRegistry` alongside the built-ins; the `ToolProviderContext` carries the turn's `org_id` + `AccessContext` so a host can return per-org tools. No provider ⇒ the registry is exactly today's built-ins.
- **Per-org agent persona** — an optional `AgentSettings.persona: Option<String>`; the runner uses the resolved persona as the turn's system prompt when present, else falls back to the existing const `KNOWLEDGE_CHAT_SYSTEM_PROMPT`. No persona ⇒ identical prompt to today.

Both seams are behavior-preserving by default — the local/default flavor is unaffected.
