---
'@smooai/smooth-operator': minor
---

fix(rust): the workflow step-attempt cap never fired — a shadowed binding fed it a permanent 0

`handler.rs` carried the step-attempt-cap capture block **twice, back to back**, with
identical comments. The two differed in exactly one field: the first took the count
from `loaded_attempts` (durable conversation metadata), the second from
`state.session_step_attempts(session_id)` (the per-pod in-memory session map). Rust
shadowing meant the second won.

That silently reverted th-c12df5, which had deliberately moved the workflow pointer
*and* its attempt count onto durable conversation storage precisely because the per-pod
map "reset them to step 0 every turn, freezing the workflow at the first step so the
judge/cap could never advance it".

It was worse than the old behaviour, though. The two stores used different metadata
keys — `persist_workflow_step` writes `workflowStepAttempts`, while the session
accessor read `stepAttempts`, a key nothing ever wrote, and its writer
(`set_session_step_attempts`) had no callers anywhere in the repo. So the surviving
binding fed `apply_step_cap` a **permanent 0**: `next_attempts` never reached
`WORKFLOW_STEP_ATTEMPT_CAP`, and a workflow step the judge never accepts could loop
forever — exactly the pathological-visitor case (th-d57a1d) the cap exists to bound.

The fix keeps the durable source and deletes the duplicate. `AppState::session_step_attempts`
and `AppState::set_session_step_attempts` are **removed** rather than left in place:
both were vestigial from the pre-th-c12df5 design, neither had a live caller, and the
getter's doc comment still advertised itself as feeding the cap. Leaving them is what
let the wrong source get wired in, so removing them makes the mistake unavailable
rather than merely un-made. This also clears three `unused variable` warnings that had
been emitted on every build.

Covered by a regression test that drives the real per-turn pipeline
(`load_workflow_step` → `apply_step_cap` → `persist_workflow_step`), reloading from
storage each turn as a reconnect or pod hop would, and asserting the held step
force-advances exactly at the cap. Verified by mutation: feeding it a zeroed count
fails the test the same way the bug did.
