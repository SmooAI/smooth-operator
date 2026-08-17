---
"@smooai/smooth-operator": patch
---

fix(go,ts,python): reject a create_conversation_session with no agentId

Mirrors core#434 (`bfd3911`). `agentId` is `required` in
`create-conversation-session.schema.json` and the generated client type is
non-optional, so absent-or-blank is a **malformed request**, not an agentless
session. The original code fabricated a UUID; th-68897a's first pass stopped
fabricating but silently stored NULL. Both accept a malformed request and differ
only in what they write down — neither validates a required inbound field.

All three now answer `VALIDATION_ERROR` / `Missing 'agentId'` and persist nothing,
using each server's existing error-emission path.

**One entry point per server, not two.** Rust needed both its WS handler and its
Lambda dispatcher; Go, TypeScript and Python have no Lambda — the only request
boundary is the dispatcher's `create_conversation_session` case. The two-store
pattern from th-68897a applied to the *fabrication*, which lived in each store;
*validation* belongs at the request boundary, and the stores have no request id or
sink to answer on.

Everything from th-68897a stays: the column is still nullable, the field still
optional, Go still uses `""`-as-absent. That remains honest for rows written before
this check — it is just no longer reachable from the create path.

**14 pre-existing tests were sending malformed creates** and are corrected here,
which is the real finding. One of them (`test_send_message_without_gateway_errors_cleanly`)
didn't fail — it **hung**, waiting forever for an `immediate_response` that is now an
`error`. The rest span conversation scoping, resume, file transfer, graceful drain,
skills, tool hooks, turn round-trip and the preamble. None of them ever needed an
agentless session; they just never had to supply the field.

Three tests per language: absent, `""` and `"   "` are each rejected **and** nothing
is persisted — a rejection that still writes a row is the same bug wearing an error
message — plus one asserting a real agentId still works.

Green, all exit 0: Go `vet` + `go test`, TypeScript `tsc` + 317 tests, Python ruff +
323 tests.
