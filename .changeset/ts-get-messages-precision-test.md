---
'@smooai/smooth-operator': patch
---

TypeScript server: regression test pinning sub-second `createdAt` precision on `get_conversation_messages`. A server that formats `createdAt` at whole-second precision breaks the documented paging loop — clients feed page one's oldest `createdAt` back as `before`, and a strictly-less-than filter against a truncated cursor silently drops every message sharing that second. The TS server was already correct (`Date#toISOString`, millisecond precision, passed through unreformatted); the test locks it in.
