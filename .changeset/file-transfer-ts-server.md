---
'@smooai/smooth-operator-server': minor
'@smooai/smooth-operator': minor
---

File transfer (TS server): implement the PR #342 contract to Rust parity. `send_message` now parses `images[]` and `files[]`: images are attached to the turn's user message as OpenAI `image_url` content parts (via a `withUserImages` request-body wrapper, since the published core takes a plain-string message and reuses it for retrieval), while files are surfaced — never sent to the model — on a new per-turn `ToolContext`. A new optional `toolProvider` seam (mirroring the Rust `ToolProvider`) hands host tools that context, including a directive sink; a host tool that writes `ctx.directive` (e.g. a `send_file` directive) has it drained onto `eventual_response.directive` (last-write-wins). All attachment parsing is fail-soft. The client SDK's generated protocol types gain the `images`/`files` request fields and the `directive` response field.
