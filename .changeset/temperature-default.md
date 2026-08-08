---
'@smooai/smooth-operator': patch
---

Send temperature 1.0, not 0.0 — many frontier models reject anything but their default

A growing set of models accept only their default temperature and 400 the entire
request ("Unsupported value: 'temperature' does not support 0 with this model").
The symptom does not look like a config error: the server boots, accepts the turn,
every LLM call 400s, and the user sees an assistant that silently says nothing.

A per-model allowlist would be provably wrong — `gpt-5.1` rejects while `gpt-5.2`
accepts, `gpt-5.4` accepts while `gpt-5.4-pro` rejects. `1.0` was accepted by all
12 models tested across 6 families, so it is the one value that works everywhere.

Centralised as `config::DEFAULT_TEMPERATURE`. The cleaner long-term shape is
`Option<f32>` on `LlmConfig`, so the request omits the field entirely and takes
each provider's own default.
