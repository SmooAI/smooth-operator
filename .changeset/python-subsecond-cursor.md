---
'@smooai/smooth-operator': patch
---

Python server: regression test pinning sub-second precision on `get_conversation_messages`' `createdAt`. The handler already emits full microsecond precision (`datetime.isoformat()` on a tz-aware UTC value), but nothing guarded it — clients page by handing the oldest `createdAt` back as `before`, and a second-truncated cursor makes the strict `<` filter drop every message sharing that second. Matches the Go (#264) and TypeScript (#273) fixes.
