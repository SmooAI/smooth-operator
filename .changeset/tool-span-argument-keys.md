---
"@smooai/smooth-operator": minor
---

SMOODEV-3364: `gen_ai.tool` spans record argument KEY NAMES, never argument values, and a host that traces its own tools can turn the runner's span off.

**Behaviour change — tool spans.** Every server's per-tool-call span recorded `gen_ai.tool.call.arguments`: the tool's JSON arguments with only secret-NAMED keys masked. Tool arguments are customer data — a CRM write carries a person's name, email, phone and address — so every exporter's trace store received that PII verbatim. A name or a note matches no pattern, so the value is no longer carried at all.

- The `gen_ai.tool` span now records **`gen_ai.tool.argument_keys`** (the sorted top-level key names, comma-joined) and **no longer records `gen_ai.tool.call.arguments`**, in the Rust server + `KnowledgeChatRuntime`, and the TypeScript, Python, Go and .NET servers. New `tool_argument_keys` / `toolArgumentKeys` / `ToolArgumentKeys` helpers share one set of test vectors: object → keys; `null` or empty → `""`; array → `<array>`; other scalar → `<scalar>`; unparseable → `<unparsed>`.
- `redact_tool_arguments` and the `GEN_AI_TOOL_ARGUMENTS` constant stay exported for hosts that reference them; no span uses them.
- **Rust: `ToolProvider::traces_own_tools()`** (default `false`). A provider whose tools emit their own `gen_ai.tool` span returns `true`, and the runner skips its span for those tools — built-in and extension tools keep theirs. A host decorator plus the runner used to emit two spans per call, doubling every tool-call count. Rust-first: only the Rust and TypeScript servers have a host tool-provider seam.
