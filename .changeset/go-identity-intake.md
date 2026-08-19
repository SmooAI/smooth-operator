---
'@smooai/smooth-operator': patch
---

go-server: port the `identity_intake` Rich Interaction kind + add the kind-routed host-effect seam.

The Go server now hosts a second Rich Interaction kind alongside `choices`:
`identity_intake` (structured name/email/phone lead capture, capability `identity_form`).
It mirrors the Rust reference: the `request_identity_intake` raise tool, a server-side
validator (required-field presence, email shape, phone → E.164 normalization, per-field
errors), and the conversational fallback directive for text-only channels.

This wave also adds the one framework piece the `choices` wave omitted: a **kind-agnostic
host-effect seam**. A kind may implement the optional `InteractionEffect` interface;
`attachInteractionEffect` runs it after a valid submit and is a no-op for kinds without one
(so `choices` is unaffected). It fires on BOTH submit paths — the rich `submit_interaction`
action (dispatcher) and the generic `submit_interaction` tool (the conversational fallback,
newly added to the turn runner). `identity_intake`'s effect stamps the captured, normalized
identity onto the session metadata (`userName` / `contactEmail` / `contactPhone`), the same
keys the pre-chat create path stashes and the OTP contact seam reads, so a captured email/
phone becomes OTP-contactable on the next turn.

Tests: validator unit tests (+ shared-fixture cross-check against the Rust reference) and a
WS park/resume integration test on both paths that asserts the host effect stamped the
session and left it OTP-contactable.
