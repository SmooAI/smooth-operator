---
'@smooai/smooth-operator': minor
---

Connection backplane (SMOODEV-1891): a pluggable `Backplane` trait + default `InMemoryBackplane` in the OSS server — the scale-out + event-delivery seam. Each connection's outbound sink is attached on connect and associated with its session/agent; `publish(Target, event)` delivers to every connection for a target. This is the foundation for running >1 replica (a Redis/NATS impl makes delivery cross-pod) and the plug point for non-AI realtime: any service can `publish(Target::Session(...), event)` and reach the connected client over WebSocket. Wired into `AppState` (`with_backplane`) + the connection lifecycle. Runtime-agnostic (the sink is a closure, no tokio dep added to the lib).
