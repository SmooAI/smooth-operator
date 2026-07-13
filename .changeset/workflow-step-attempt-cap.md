---
"@smooai/smooth-operator": patch
---

Add a per-step attempt cap to the conversation-workflow judge so a guided assessment can't stall forever on one step. The judge only advances on `yes`; when a step's criteria demand evidence the judge never accepts, the step re-asks indefinitely and a multi-step flow (e.g. the public Transformation Posture agent) never reaches its scoring / lead-capture step (th-d57a1d). The step pointer already persists and advances correctly — this adds the missing escape hatch: `apply_step_cap` force-advances to the next step after `WORKFLOW_STEP_ATTEMPT_CAP` (3) consecutive non-advancing turns, resetting the counter on any advance. The counter persists in session metadata (`stepAttempts`) alongside the existing `currentStepId` pointer. With tuned criteria the cap rarely fires — it's the safety net for a pathological non-answering visitor.
