---
'@smooai/smooth-operator': patch
---

Go server: emit `createdAt` with sub-second precision (`RFC3339Nano`) from `get_conversation_messages`. Clients page by handing the oldest `createdAt` back as `before`, which is filtered strictly-less-than against the store's full-precision timestamp — whole-second `RFC3339` truncation put the cursor *before* the message it named, so every message sharing that second silently vanished from page two. Also aligns the Go wire format with the .NET server, which already round-trips full precision.
