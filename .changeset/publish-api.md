---
'@smooai/smooth-operator': minor
---

Realtime publish endpoint (SMOODEV-1893): `POST /admin/publish` lets non-AI publishers — job status, ingestion progress, notifications, billing — push an event to a backplane target over the WebSocket fleet without going through an agent turn. Body is `{ target: { type: session|user|org|agent|connection, id }, event }`; it calls `Backplane::publish`, so with a distributed backplane the event fans out across pods. Admin-gated (RBAC role 2); the response reports local deliveries on the serving pod (cross-pod deliveries happen but aren't counted). Targets are opaque ids matched against the connection registry — tenant id-namespacing is a host concern, documented on the handler.
