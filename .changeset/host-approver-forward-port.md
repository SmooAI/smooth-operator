---
'@smooai/smooth-operator': patch
---

feat(rust): forward-port the host-approver seam + its drain fix to main

`runner::HostApprover` and `LocalServerBuilder::host_approver` (th-be3f55), plus
the one-drain-task starvation fix that followed them (th-2105e9), only ever
existed on the `th-daemon-memory-seam` side lineage. smooth's daemon consumes
both, which is why smooth pinned `smooth-operator-server`/`-svc` to a git rev off
that branch — and that rev predates the `..` fixes, so it cannot compile against
core >= 1.7.3. Pinning the branch was what pinned core at `=1.7.2`.

Bringing both commits onto main dissolves that side lineage, so smooth can point
at main and take current cores.

**th-be3f55 — host-approver seam.** The companion to `tool_hooks`: that seam lets
a host install a permission gate, but a gate that can only allow or deny is a gate
that must run in Bypass. Supplying the receiving ends of the hook's approver
channel gives its `Ask` the same treatment the tool-pattern HITL already has — the
turn parks, the client is sent `confirm_tool_action_required`, and
`confirm_tool_action` resumes it. Unset ⇒ unchanged (a host `Ask` still fails
closed). The confirmation config is now built when EITHER tool patterns are
configured OR a host approver is supplied; it was patterns-only before.

**th-2105e9 — one drain task, not one per turn.** Each turn's bridge locked the
shared request receiver for its whole lifetime, so a turn parked awaiting a human
held it and starved every other turn on the connection.
