---
"@smooai/smooth-operator": minor
---

feat(server): `confirm_tool_action` — write-confirmation human-in-the-loop pause/resume

The reference WebSocket server can now gate write tools behind human approval.
When an agent turn calls a tool whose name matches `SMOOTH_AGENT_CONFIRM_TOOLS`
(comma-separated substrings), the turn **parks** and emits a
`write_confirmation_required` event (matching
`spec/events/write-confirmation-required.schema.json`) carrying
`{ toolId, actionDescription }`. The client resumes it by sending
`confirm_tool_action` (`{ sessionId, requestId, approved }`, per
`spec/actions/confirm-tool-action.schema.json`): on `approved: true` the parked
tool executes; on `false` it is skipped with a rejection result the model sees,
and the turn still completes.

Built entirely on the existing smooth-operator-core human-gate primitive
(`ConfirmationHook` + `human_channel()` + `AgentConfig::with_human_channel`) —
**no core change required**. The server wires the hook's `HumanRequest` stream to
a WS event and bridges an inbound `confirm_tool_action` back to the hook's
`HumanResponse`, keyed by session. The `send_message` turn now runs in a spawned
task so the socket reader stays free to receive the confirmation on the same
connection (the turn would otherwise deadlock awaiting a frame it is blocking).

With `SMOOTH_AGENT_CONFIRM_TOOLS` unset (the default), no `ConfirmationHook` is
installed, no tool ever parks, and behavior is byte-for-byte unchanged. The
local/default flavor is unaffected.
