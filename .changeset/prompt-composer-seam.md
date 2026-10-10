---
'@smooai/smooth-operator': minor
---

SMOODEV-3798: a `PromptComposer` seam assembles every turn's system prompt from its ordered sections.

The runner hard-coded `base → first-turn greeting → workflow step → skill → suggested-replies trailer`, so a host could only change `base` (via agent instructions, the org persona or the default persona). A host that must put a section LAST on every turn (a platform safety block that no agent prompt, greeting or workflow step can override by recency), render its own section from who is asking, or reorder had nowhere to do it. An agent with no instructions never reached a host fold at all.

- New `PromptComposer` trait: `compose(&PromptSections) -> Vec<String>`. The runner drops blank sections and joins the rest with `"\n\n"`.
- `PromptSections` carries every rendered section (`base`, `greeting` on the first turn only, `workflow`, `skill`, `suggested_replies`), plus `base_source` (`Agent` / `OrgPersona` / `DefaultPersona` / `BuiltIn`), the answering agent's `AgentBehaviorConfig`, the requester's `AccessContext`, `session_authenticated` and `conversation_id`.
- `DefaultPromptComposer` keeps the historical order. Hosts that only append can call `DefaultPromptComposer::sections` and push onto it.
- Install it with `AppState::with_prompt_composer` or `LocalServerBuilder::prompt_composer`. With none installed, the prompt is unchanged, except that a blank workflow section no longer leaves an empty gap.
- Rust: `TurnRequest` gains `prompt: TurnPrompt`; struct literals add `prompt: Default::default()`.
