---
'@smooai/smooth-operator': patch
---

SMOODEV-3630: Require `smooai-smooth-operator-core` 1.14.1, the first core that stops replaying assistant `reasoning_content` to `groq-*` models. Groq rejects that field with a 400, and the gateway then answers from a fallback model, so on 1.10.0 every multi-turn Groq conversation after the first reply was served by a different model.
