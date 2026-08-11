---
'@smooai/smooth-operator-server': minor
---

Python server: file-transfer parity with the Rust reference. `send_message.images[]` now attach to the model's user message as OpenAI `image_url` content parts (multimodal turns), `send_message.files[]` are surfaced on the per-turn tool-provider context (a host tool lands them in the workspace; never sent to the model), and a host tool can write a client-side directive (e.g. the `send_file` convention) onto the turn's directive sink — drained after the turn onto `eventual_response.directive`. All fail-soft and back-compatible: a text-only turn with no attachments is byte-identical to before. Regenerated the Python SDK types from the updated spec (`files[]` + directive docs).
