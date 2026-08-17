---
'@smooai/smooth-operator': patch
---

Make `agentId` optional in the session schemas.

th-68897a stopped fabricating an `agentId` when the caller names no agent, but the spec still
listed it as required — so after that change there was no spec-valid way to describe an
agentless session: `null` fails the type, omission fails `required`. Every server emitting the
honest shape was technically out of spec.

`agentId` is dropped from `required` in `spec/domain/session.schema.json` and
`create-conversation-session.schema.json#/$defs/Response`. It keeps its type for when present;
absence is represented by omitting the field, which is already what the Rust and .NET servers
emit.
