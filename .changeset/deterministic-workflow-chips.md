---
"@smooai/smooth-operator": patch
---

Deterministic workflow chips (th-d57a1d). `ConversationWorkflowStep` gains an optional `suggestedReplies: string[]`; when the agent is on a step that declares it, the server emits those canonical answers as the response's `suggestedNextActions`, overriding any model-invented chips. This makes quick-reply chips fire on every such step (reliable, not model-dependent) and — because a tapped chip is clean, canonical input — fixes the assessment stalling where the judge would not advance on terse free-text answers. Free-form steps declare none, leaving model behavior unchanged.
