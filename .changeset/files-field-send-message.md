---
'@smooai/smooth-operator-server': minor
---

`send_message` grows an optional `files[]` array — non-image attachments the host persists, never sent to the model.

Each entry is `{ name, mimeType?, url }`. Parsing mirrors `images`: fail-soft (an absent key ⇒ empty, a malformed array is dropped rather than rejecting the turn). Unlike `images`, files do NOT reach the model — they ride the turn's `ToolProviderContext` only, so a host tool (paired with the `send_file` directive convention on `eventual_response.directive`) can persist and echo them back.

- New `smooth_operator::tool_provider::UserFile { name, mime, url }` (`mimeType` ↔ `mime` on the wire), alongside `UserImage`.
- `ToolProviderContext.files` + `with_files(..)` builder; threaded from the `send_message` frame through `TurnRequest` into the per-turn tool-provider context.

Backward compatible: an absent `files` field is byte-for-byte the previous behavior. Rust is the reference implementation.
