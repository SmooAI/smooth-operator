---
"@smooai/smooth-operator": patch
---

Persist conversation-workflow step state to shared storage (th-c12df5). The step pointer (`currentStepId`) and per-step attempt counter were held in the per-pod in-memory session map, so on a widget reconnect or a pod hop they reset to step 0 — the workflow froze on its first step, the judge/attempt-cap could never advance it, and any per-step rich elements (quick-reply chips today, richer message elements later) were pinned to that first step. They now live on the conversation's `metadata_json` (shared storage, keyed by the stable `conversation_id`) and load per turn, so a workflow resumes on the right step across reconnects and replicas. Element-agnostic — the fix moves the step pointer, not the emitted content.
