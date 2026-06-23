---
'@smooai/smooth-operator': minor
---

Phase 2: human-in-the-loop approval (HumanGate) across the Python, TypeScript, and
Go cores, at parity with the C# reference. The agent consults an optional approval
gate before running any tool flagged by a `requires_approval` predicate; a denial is
fed back to the model as the tool result (the tool never runs) and an approval lets
it execute normally. With no gate configured, behavior is unchanged.
