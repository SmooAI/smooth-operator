---
"@smooai/smooth-operator": minor
---

feat(server): derive org + agent from auth in `create_conversation_session`

`handle_create_session` no longer hard-codes the seed org. It now derives the
session's `organization_id` from the authenticated request, in priority order:

1. the agent's widget-auth policy `organization_id` (widget visitors authenticate
   via origin + `authContext`, not a JWT, so their org rides on the agent policy —
   new optional `AgentWidgetAuth.organization_id` field),
2. the connection's authenticated JWT principal org (dashboard / authed clients —
   the principal's `org_id` is now threaded from the `/ws` handshake through to the
   handler instead of being dropped at `AccessContext` reduction),
3. the server's seed org as a behavior-preserving fallback for the no-auth/local
   flavor.

The agent id continues to come from the inbound `agentId` payload. The same
JWT-org-then-configured-org derivation is applied to the lambda dispatch
create-session path. All existing in-memory/seed flows are unchanged.
