---
'@smooai/smooth-operator-js': minor
---

SMOODEV-3710: `list_conversations` reads the caller's own conversations in one storage call when the host requires owned conversations.

The generic listing read every conversation in the org — phone calls, SMS and widget chats included — then issued a participant read and a message read per conversation, so an org with thousands of conversations paid thousands of serial round trips every time the history picker opened. With `with_require_owned_conversations(true)` the caller can only ever see what it owns, so the handler now asks the adapter for exactly that through the new `StorageAdapter::list_owned_conversation_summaries(org, email, limit)`.

The default implementation composes the existing reads (correct for any adapter); a backend that can answer in one query should override it. The rows are identical to the scan's: owned, non-empty, newest first, same title and count. An emailless principal lists nothing, as before. Hosts without the flag are unchanged.

Also adds `MessageContent::flat_text()`.
