---
'@smooai/smooth-operator': minor
---

Reject `create_conversation_session` when `agentId` is absent or blank.

`agentId` is required by the Request schema and the generated client type is non-optional, so
an absent or blank one is a malformed request — not an agentless session. The original code
fabricated a UUID for it; th-68897a's first pass stopped fabricating but silently stored NULL.
Both skip the validation that belongs at that boundary. It is now a `VALIDATION_ERROR`, in
both the WebSocket handler and the Lambda dispatcher.

The column and field stay nullable. That is th-68897a's real win — nothing is ever invented —
and it remains honest for rows written before this validation existed. It is simply no longer
reachable from the create path.
