---
'@smooai/smooth-operator': patch
---

dotnet-server: port the `identity_intake` Rich Interaction kind + add the kind-routed host-effect seam.

The .NET server now hosts a second Rich Interaction kind alongside `choices`:
`identity_intake` (structured name/email/phone lead capture, capability `identity_form`).
It mirrors the Rust reference: the `request_identity_intake` raise tool, a server-side
validator (required-field presence, email shape, phone → E.164 normalization, per-field
errors reported one-pass), and the conversational fallback directive for text-only channels.

This wave also adds the one framework piece the `choices` wave omitted: a **kind-agnostic
host-effect seam**. `IInteractionKind` gains an optional `ApplyEffect` hook (no-op default,
so `choices` is unaffected); the caller resolves the kind from the DI-provided
`InteractionCatalog` and runs its effect without knowing which kind it is. It fires on BOTH
submit paths — the rich `submit_interaction` frame (`FrameDispatcher`) and the generic
`submit_interaction` tool (the conversational fallback, wired through the turn runner).
`identity_intake`'s effect stamps the captured, normalized contact onto a session-keyed
in-memory overlay (`SessionIdentityRegistry`) — the C# analog of the Rust reference's
in-memory session metadata (`userName` / `contactEmail` / `contactPhone`) — which the OTP
contact seam now reads alongside the create-session email, so a captured email/phone (phone →
SMS) becomes OTP-contactable on the next turn.

Tests: validator unit tests (+ shared-fixture cross-check against the Rust reference) and WS
park/resume integration tests on both the rich and conversational-fallback paths that assert
the host effect stamped the session and left it OTP-contactable.
