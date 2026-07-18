---
'@smooai/smooth-operator': minor
---

Add a host-callable seam to start an agent turn server-side (`IServerInitiatedTurns`, registered by `AddSmoothOperatorServer`). A host — e.g. `POST /webhooks/datadog` saying "investigate this alert" — can now create a conversation and run a turn without a client `send_message` frame. It reuses the same `TurnRunner` + `ISessionStore` path as the client flow, so the inbound message and streamed reply persist identically: a client that later lists or resumes that conversation sees it the same as a client-initiated one. Interactive per-connection concerns (write-confirmation HITL, OTP gating) are intentionally omitted. Live push to already-connected sockets is deferred — the durable message log is the surface clients read.
