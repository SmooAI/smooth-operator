---
'@smooai/smooth-operator': patch
---

fix(rust): add `..` to the last four exhaustive `AgentEvent` matches

#328 added `..` to the two `ToolCallComplete` matches that core 1.7.3's new
`details` field actually broke. Four sibling matches on the same enum were still
exhaustive, so the NEXT field added to any of those variants breaks every
consumer that unifies the dependency graph on a newer core — the same failure,
just deferred:

- `smooth-operator/src/runtime.rs` — `ToolCallStart` (in `tool_arguments_for`)
- `smooth-operator-server/src/runner.rs` — `TokenDelta`, `ReasoningDelta`, `ToolCallStart`

These compile fine today; the fix is purely so they keep compiling. Construction
sites in tests are deliberately left alone — `..` doesn't apply there, and a new
field should break a mock that claims to build a complete event.
