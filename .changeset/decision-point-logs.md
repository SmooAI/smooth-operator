---
"@smooai/smooth-operator": patch
---

fix(server): a prod failure is diagnosable from logs alone (th-694c22)

A live "session not found" incident produced ZERO server log lines — chat-ws
emitted one line in six hours of continuous traffic, because the server's
decision points were silent: every one of the ~30 `protocol::error(...)` emit
sites sent the client-visible error frame without logging, and the session
read-through, confirmation/interaction parks and resolves, OTP verification,
and turn starts logged nothing at info.

One warn at the single error-frame construction site now covers every
client-visible failure, present and future (the frame's human text rides as
`detail` — `message` is tracing's reserved event-message field). Info lines
land at the decision points an incident responder actually needs: session
primed from storage (the cross-pod resume working as designed), turn requested
(session + requestId), confirmation parked / live-resolved / durably resolved,
interaction parked / resolved-with-values, and OTP verified. No debug spam —
one line per event that changes state.
