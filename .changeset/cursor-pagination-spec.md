---
'@smooai/smooth-operator': minor
---

Protocol: `get_conversation_messages` pagination moves from a timestamp cursor to an opaque cursor.

`before` (ISO 8601 timestamp) is replaced by `cursor` (opaque, storage-defined — a message id today), and the response gains `nextCursor`, non-null exactly when `hasMore` is true. Page by feeding `nextCursor` back as the next request's `cursor`.

A timestamp is the wrong cursor: two messages can share a timestamp at any precision the wire format preserves, so a `created_at < cursor` filter either drops or repeats the messages that collide. This is not hypothetical — the Go server shipped whole-second `RFC3339` and silently dropped every message sharing a second from page two. An id names exactly one message and cannot collide. The Rust server already paginated this way; this makes the spec match the design that was already correct.

Breaking on paper, inert in practice: a survey of every consumer (smooai, smooth, heypage) found no caller that pages — all are single-fetch, none passes `before`, none reads `hasMore` or `nextCursor`.

Also regenerates all client type sets from `spec/`, which had drifted badly. The regen pulls in schema changes that landed without regeneration (`cancel`, `submit_interaction`, `interaction_required`/`interaction_invalid`) and surfaces a latent bug: `organizationId` became required on `Session` in spec PR #97, but Python's model was never regenerated, so the Python client has been accepting sessions a conformant server would reject ever since.
