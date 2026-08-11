---
'@smooai/smooth-operator-server': minor
---

.NET server: implement the file-transfer contract (PR #342) — `send_message.images[]` / `files[]` and the `send_file` directive.

The C# server was text-only on `send_message`; it now reaches Rust parity:

- **`images[]`** — parsed fail-soft and attached to the turn as OpenAI `image_url` content parts. Because the engine builds the live user turn from a string (no content parts), the images ride on an image-only `ChatMessage` seated on the thread immediately before the text turn — so the model sees them adjacent to the question, with no empty or duplicated message. A `data:` URL becomes a `DataContent`, an `http(s)` URL a `UriContent`; the optional vision `detail` hint rides on `AdditionalProperties`. Malformed/unsupported entries are dropped, never rejecting the turn.
- **`files[]`** — parsed fail-soft and surfaced on a new per-turn `TurnContext` (files list + directive sink), published as an `AsyncLocal` around the run so a host tool the engine invokes can read them. Never sent to the model.
- **`send_file` directive** — a host tool writes an opaque directive onto `TurnContext.Current.Directive`; the runner drains it after the turn onto `eventual_response.directive` (last-write-wins, omitted when none for back-compat).
- Generated `Types.cs` regenerated from the updated spec (adds `SendMessageRequest.Files`, `Skill`).

Backward compatible: absent `images`/`files` and no host directive is byte-for-byte the previous text-only behavior. Source-only — the engine stays the published NuGet.
