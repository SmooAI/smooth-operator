---
'@smooai/smooth-operator': minor
---

feat(server): graceful SIGTERM/ctrl_c drain of WebSocket connections.

The reference WebSocket server (`smooth-operator-server`) now drains in-flight
turns on shutdown instead of being killed mid-flight. Previously `run()` did a
plain `axum::serve(listener, app).await` with no `with_graceful_shutdown`, so on
a Kubernetes pod termination (scale-down / rollout) the process was killed while
turns were in progress — in-flight WebSocket turns dropped and connections never
`detach`ed from the `Backplane`, leaving stale registry entries in Valkey/NATS.

A single shared `tokio_util::sync::CancellationToken` is now threaded through
`AppState` (`shutdown`, defaulted to a fresh never-cancelled token in
`AppState::new`, plus a `with_shutdown` builder). Each per-connection reader loop
`select!`s on that token (`biased`, shutdown wins ties) with the inbound-frame
read — and keeps `handle_frame(...).await` inside the frame arm so a turn already
in flight finishes before the next shutdown check. After the loop the existing
`backplane.detach(...)` runs, so the connection always leaves the registry clean.
The serve loop (`run`) wires `axum::serve(...).with_graceful_shutdown(...)` to
SIGTERM (k8s) or ctrl_c (interactive), cancelling the token to fan the drain out
to every connection within the chart's `terminationGracePeriodSeconds` window.
