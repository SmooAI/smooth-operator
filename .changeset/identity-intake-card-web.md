---
"@smooai/smooth-operator": minor
---

feat(web): `identity_intake` Rich Interaction card (name/email/phone lead capture) for the React SDK + web-chat example

The `identity_intake` interaction kind (structured name/email/phone lead capture,
capability `identity_form`) now has a web renderer. `IdentityIntakeCard` (exported
from `@smooai/smooth-operator/react`) renders one labelled input per `spec.fields`
in order — `name` → text, `email` → email, `phone` → tel — marking required
fields, focusing the first input on mount, and honoring a per-field `label`
override. Submit builds the canonical `{ name?, email?, phone? }` values (only the
fields the visitor filled in, trimmed) and resumes the parked turn via the existing
`submitInteraction()` verb; a Decline path sends `declined: true`. Server-side
`interaction_invalid` errors re-render per field (bad email, missing required,
phone-normalization message) with the turn still parked and the input flagged
`aria-invalid` + wired to the error via `aria-describedby`.

The `interactionCards` registry (`kind` → card) now carries both `choices` and
`identity_intake`; it moved to its own `components/interactionCards.ts` module so
each card file owns only its card, and its value type is a loose common
`InteractionCardProps` so a dynamic `interactionCards[kind]` lookup renders any
kind with no per-kind code. The web-chat example declares the `identity_form`
capability alongside `choice_chips` in `create_conversation_session`, and its
existing overlay-slot lookup renders the card unchanged.

No protocol/client change — the generic `submit_interaction` verb already speaks
every kind.
