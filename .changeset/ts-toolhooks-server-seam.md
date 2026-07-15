---
"@smooai/smooth-operator": patch
---

feat(ts server): `toolHooks` seam plumbs consumer-supplied ToolHooks into every turn's tool registry

The TypeScript server gains a `toolHooks` option on `ServerOptions` (and
`serveLocal`), forwarded verbatim through `FrameDispatcher` → `TurnRunner` →
the engine's `AgentOptions.toolHooks`. Consumer-supplied `ToolHook`s run around
every dispatched tool: `preCall` before execution (a throw blocks the call) and
`postCall` after with a mutable result it may redact. Unlike `tools`, hooks
bypass the per-agent enabled-tools filter and auth gating — they observe/redact
every call. Empty ⇒ behaviour unchanged. This is the server half of the
polyglot ToolHook parity work, mirroring the Rust `LocalServerBuilder` hook seam
feeding the per-turn `ToolRegistry`. Requires `@smooai/smooth-operator-core`
with the `ToolHook` lifecycle.
