---
'@smooai/smooth-operator': minor
---

SEP — the .NET operator server (`dotnet/server`) now hosts extensions (ui/confirm producer).

The C# server wires the engine `ExtensionHost` (from `SmooAI.SmoothOperator.Core` 1.4.0)
into each `send_message` turn. With `SMOOTH_EXTENSIONS_ALLOW` set (a default-deny allowlist —
the server has no interactive trust prompt), `ExtensionServerHost.BuildAsync` discovers
`extension.toml` extensions, spawns them as JSON-RPC/ndjson subprocesses, and exposes their
tools. Those tools join the turn's tool set so they flow through the SAME per-agent
`enabled_tools` filtering + auth gate as native tools (dotted `<ext>.<tool>` names match
`toolId`), and the host is torn down (subprocesses killed) at turn end.

An extension's `ui/confirm` bridges onto the operator protocol's
`write_confirmation_required`/`confirm_tool_action` frames via `ConfirmUiProvider` — parking
on the same session-keyed `ConfirmationRegistry` the native write-tool HITL uses. Every other
`ui/*` degrades headless. Only the `confirm` capability is advertised at handshake.

Additive: with the allowlist empty (the default) no host is ever built, so behavior is
byte-for-byte unchanged. Verified by an integration test that runs the spec's Node echo peer
through a real server turn and asserts `enabled_tools` filtering drops an extension tool
exactly like a native one.
