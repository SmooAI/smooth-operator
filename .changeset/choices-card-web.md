---
"@smooai/smooth-operator": minor
---

feat(web): `choices` Rich Interaction card (AskUserQuestion) for the React SDK + web-chat example

The `choices` interaction kind (structured multiple-choice ask, modeled on Claude
Code's AskUserQuestion) now has a web renderer. `ChoicesCard` (exported from
`@smooai/smooth-operator/react`) renders each question's `header`, prompt, and
option chips — radios when `multiSelect` is false, checkboxes when true — plus a
free-text **"Other"** escape hatch per question that is always available. Submit
builds the canonical `{ answers: [{ header, options?, other? }] }` values and
resumes the parked turn via the existing `submitInteraction()` verb; a Decline
path sends `declined: true`. Server-side `interaction_invalid` errors re-render
per question with the turn still parked.

A minimal `interactionCards` registry (`kind` → card) is exported so a client
looks the card up by kind; `choices` is registered there. The web-chat example
declares the `choice_chips` capability in `create_conversation_session` and
renders the card in its overlay slot above the composer.

Also regenerates `src/generated/types.ts` from `spec/` (adds `ChoicesSpec` /
`ChoicesValues` / `ChoicesPayload`; picks up the already-merged optional-`agentId`
and `choice_chips` spec descriptions). No protocol/client change — the generic
`submit_interaction` verb already speaks every kind.
