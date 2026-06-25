---
"@smooai/smooth-operator": minor
---

Add `organizationId` to the `Session` domain type so org-scoping is uniform across every core domain type (`Conversation`, `Participant`, and `Message` already carry it). Storage backends can now write the session's org directly instead of re-deriving it from the conversation. The built-in Postgres adapter gains an `organization_id` column (additive, `DEFAULT ''`) on `conversation_sessions` plus an org index; the in-memory and DynamoDB adapters thread the new field through automatically; server/runner create-session paths populate it from the conversation/turn org already in scope.
