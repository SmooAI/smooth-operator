---
'@smooai/smooth-operator': minor
---

SEP Phase 6 (chat-widget) — render agent confirmation prompts as chat-native
buttons.

The embeddable chat widget now renders a `write_confirmation_required` HITL
event as an inline Yes/No button prompt inside the assistant bubble instead of
silently ignoring it. Clicking a button sends the `confirm_tool_action` resume
frame and un-pauses the turn; the chosen answer sticks in the transcript. This
is the chat-native projection of SEP `ui/confirm` (a hosted extension's confirm
prompt maps onto the existing `write_confirmation_required` frame).

`ConversationController` gains `answerPrompt(requestId, value)` and an optional
client-options constructor arg (a transport seam for tests). `ChatMessage` gains
an optional `prompt` field (`ChatPrompt`) carrying the buttons; the multi-option
shape also backs a future `ui/select` chat frame.
