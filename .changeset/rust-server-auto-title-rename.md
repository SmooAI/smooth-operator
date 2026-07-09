---
'@smooai/smooth-operator': patch
---

Rust server: conversation auto-title (small model) + `rename_conversation` (pearl th-d5b446).

- **Auto-title** — after the first assistant turn on a conversation still carrying its default `Session <uuid>` name, a best-effort, detached, non-blocking task asks the fast/cheap `groq-gpt-oss-20b` model for a short 3-6 word title over the first exchange and stores it as the conversation `name`. Fail-safe: any error (no gateway key, gateway failure, empty output, storage error) simply leaves the default name — a turn is never slowed or broken. The default-name guard (re-checked right before the write) means a manual rename is never clobbered, and a titled conversation won't re-fire.
- **`rename_conversation`** — new WS action `{action, requestId, conversationId, title}`: sanitizes/trims the title (rejects empty), 404s an unknown conversation, persists `name` via the storage adapter's existing `update_conversation`, and replies `immediate_response` (200) with `{ conversationId, title }`.
- `list_conversations` now surfaces a **meaningful** conversation `name` (auto-title or manual rename — anything not the default `Session <uuid>`) as the sidebar title, falling back to the first-inbound message preview for un-titled conversations. Back-compat: every pre-titling conversation carried the default name, so the message-preview behavior is unchanged for them.

Additive + back-compat. New tests cover title sanitization (quotes/markdown/whitespace/length), the default-name-only auto-title guard (mock gateway, never clobbers a manual name, no-key fail-safe), rename success + list surfacing, empty-title rejection, and unknown-id 404.
