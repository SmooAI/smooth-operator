---
"@smooai/smooth-operator": minor
---

Two additive SEP-protocol enhancements on the streaming path (directive nav + business-card images), both optional and back-compatible.

**Directive-over-SEP.** `eventual_response` gains an optional `directive` field — an opaque client-side directive (e.g. a Navigate / ApplyView instruction) a host tool emitted this turn. The runner threads a `directive_sink` into the `ToolProviderContext` (new `with_directive_sink` builder), drains it after the turn (last-write-wins, mirroring the citation sink), and carries the value onto `TurnResult::directive`. The protocol layer never interprets the shape — the host client owns it, exactly like `response`. Absent when no host tool wrote one, so the event is byte-for-byte unchanged for existing clients. Added to `spec/events/eventual-response.schema.json` and `spec/actions/send-message.schema.json` `$defs/Response`, and to the TypeScript SDK.

**Image-through-SEP.** `send_message` gains an optional `images` array (`{ url, detail? }`) for multimodal turns. A new facade `UserImage` type flows from the inbound request into `TurnRequest::images` and the `ToolProviderContext` (new `with_images` builder); when non-empty the runner maps each onto a core `ImageContent` and attaches them to the engine's user message via `AgentConfig::with_user_images` (requires core `0.16.2`). Parsing is fail-soft (a malformed `images` entry is dropped, never rejects the turn). Empty/absent ⇒ a text-only turn, unchanged. Added to `spec/actions/send-message.schema.json` `$defs/Request` and the TypeScript SDK.
