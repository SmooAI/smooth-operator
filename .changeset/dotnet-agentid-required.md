---
"@smooai/smooth-operator": patch
---

fix(dotnet): reject `create_conversation_session` without an `agentId` (th-68897a)

Follow-up to the P1 slice. `agentId` is REQUIRED by the Request schema and the generated
client type is non-optional, so absent-or-blank is a **malformed request**, not an
agentless session. The old code fabricated a UUID; P1's first pass stopped fabricating
but silently stored NULL. Both accept a malformed request and differ only in what they
write down — neither validates a required inbound field.

Both .NET entry points now reject, reusing the existing error path: the WebSocket
`create_conversation_session` handler emits `VALIDATION_ERROR "missing 'agentId'"`, and
`IServerInitiatedTurns.StartTurnAsync` throws `ArgumentException`. Both reject **before
the store is touched**, so a rejected request persists nothing — a rejection that still
writes a row is the same bug wearing an error message.

The nullable column and property from the P1 slice stay: honest for rows predating this
check, just no longer reachable from the create path.

Seventeen test frames across eleven files created sessions without an `agentId` and now
supply one. Two were relying on the absence in a way worth naming: `OtpTests` passed an
explicitly blank `"agentId":""`, and the extension tool-filter tests leaned on the
fabricated GUID resolving against a permissive config resolver.

534 pass.
