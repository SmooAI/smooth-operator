# Rich Interactions — structured interaction cards, normalized across channels

**Status: Accepted** (framework + the first kind, `identity_intake`, implemented in Rust in this repo; parity servers tracked as follow-ups). This doc is the design record (the repo has no numbered-ADR convention; Architecture docs serve that role).

## Problem

Agents constantly need **structured input** from the visitor mid-conversation — contact details, a date, a choice from a menu, a file, a rating. Today each need is improvised per channel: the chat widget has an all-or-nothing pre-chat form, SMS/voice agents freestyle the asks with no server-side validation, and every new structured need would grow its own bespoke events, verbs, and UI.

## Decision: ONE pattern — "Rich Interactions"

A **Rich Interaction** is a typed, server-validated ask the agent raises mid-turn. On a channel whose client can render it, it appears as a **rich card** (inline form / picker / chips) and the turn parks until the visitor answers. On a text-only channel the SAME raise degrades to a **conversational fallback**: kind-specific instructions the model follows turn by turn, submitting through a generic validated tool. **Either way the turn resumes with the same canonical, server-validated payload** — the agent's downstream flow is channel-independent.

`identity_intake` (name/email/phone lead capture) is the first kind and the reference implementation. Candidate future kinds the shape is proven against on paper: **date/appointment picker**, **choice chips / menus**, **file upload**, **address input**, **rating / CSAT**, **payment handoff**, **e-sign**.

### Wire surface (generic envelope, typed kinds)

One event family + one resume verb serve **every** kind:

```jsonc
// server → client (only on sessions that declared the kind's capability)
{ "type": "interaction_required", "requestId": "…",
  "data": { "requestId": "…", "data": {
    "interactionId": "…",          // server-generated instance id, echoed on submit
    "kind": "identity_intake",     // selects the client card + the server validator
    "spec": { "fields": [ { "key": "email", "required": true, "label": "Work email" } ] },
    "reason": "to send you the quote" } } }

// client → server (the ONE resume verb, all kinds)
{ "action": "submit_interaction", "sessionId": "…", "requestId": "…",
  "interactionId": "…", "kind": "identity_intake",
  "values": { "email": "a@b.com" } }        // or  "declined": true

// server → client on failed validation (turn STAYS parked; card re-renders)
{ "type": "interaction_invalid", "requestId": "…",
  "data": { "requestId": "…", "data": {
    "interactionId": "…", "kind": "identity_intake",
    "errors": [ { "field": "email", "message": "must be a valid email address" } ],
    "message": "Some fields need attention." } } }
```

**Why a generic envelope instead of typed-per-kind events** (`identity_intake_required` …): the deciding cost is the **client release train**. With typed events, every new kind needs new event/action schemas, new client-library methods, and a published client + widget release before any server can use it. With the generic envelope, a new kind is server-side only — the client's `submitInteraction()` verb and the widget's card registry already speak it; an old client simply never declares the new kind's capability and gets the conversational fallback. Per-kind precision is not lost: it moves to `spec/interactions/<kind>.schema.json` (Spec / Values / Payload shapes) and to the per-kind **raise tools**, whose LLM-facing parameter schemas stay exact. `interaction_invalid` mirrors `otp_invalid` (retryable — never a terminal `error`, which aborts a client turn).

### Capability negotiation (per-kind list)

The client declares what it can render at session create:

```jsonc
{ "action": "create_conversation_session", "agentId": "…",
  "supports": ["identity_form" /*, "date_picker", "file_upload", …*/] }
```

Each kind names the capability that gates its rich path (`identity_intake` → `identity_form`). The server decides **per kind**: capability declared ⇒ parked card; otherwise ⇒ that kind's conversational fallback. Unknown values are kept and ignored (forward-compatible). Text channels (SMS/voice) declare nothing and inherit the fallback for every kind automatically.

### Server: the `InteractionKind` extension seam (Rust reference)

All park/resume/event/registry machinery is shared and kind-agnostic. A kind supplies exactly what differs (`smooth_operator::interaction::InteractionKind`):

| Trait surface | Role | identity_intake |
| --- | --- | --- |
| `kind()` / `capability()` | identity | `identity_intake` / `identity_form` |
| `tool_schema()` + `parse_request()` | the per-kind **raise tool** (precise LLM parameter schema) → canonical `spec` + `reason` | `request_identity_intake { fields, reason }` |
| `validate(spec, values)` | **server-side validator** → canonical values or per-field errors (shared by the card path's WS handler and the fallback path's tool) | required fields, email shape, phone → E.164 |
| `fallback_directive(spec, reason)` | **conversational degradation** for text channels | "ask ONE field at a time … submit via `submit_interaction`" |

Fallback strategies are per kind by design: identity = field-by-field collect+validate; choices = enumerated ask; date = natural-language date accepted by the kind's validator.

**Adding a kind = one module + three registrations:**

1. `spec/interactions/<kind>.schema.json` (Spec / Values / Payload `$defs`) + conformance fixtures;
2. an `InteractionKind` impl (validator + fallback + raise-tool schema), registered in the server's `InteractionRegistry` (default registry = the reference catalog);
3. a widget card (see below). Nothing else: no new events, no new actions, no client-library change.

Shared machinery (kind-agnostic, one implementation): the raise tool parks via a channel pair (the `ConfirmationHook` pattern), the runner's **interaction bridge** mints an `interactionId`, registers the pending park (`sessionId → {interactionId, kind, spec, responder}`) and emits `interaction_required`; the WS handler routes `submit_interaction` to the parked kind's validator (invalid ⇒ `interaction_invalid`, still parked; `interactionId` mismatch ⇒ rejected, still parked), runs the kind's host effect, and resumes. On the fallback path the generic `submit_interaction` **tool** routes to the same validator (a same-turn raise stashes its `spec` so required-ness is enforced; a prior-turn raise degrades to format-only validation) and fires the same host-effect seam.

The canonical resume payload is framework-owned and uniform: `{ "status": "submitted", "values": <kind canonical> }` / `{ "status": "declined" | "no_response", "message": … }` — a card timeout or decline never fails the turn.

### Widget: the card registry

The widget keeps a **kind → card component** registry; `interaction_required` looks up the card by `kind` and renders it in the overlay slot above the composer. The identity card reuses the pre-chat form's field pattern (same classes, same libphonenumber as-you-type phone formatting) with per-field server errors, a submit, and a decline affordance. The widget derives its `supports` list from the registered cards, so registering a card IS declaring the capability. Adding a kind = one card component + one registry entry.

The existing **OTP** and **tool-approval** overlays are prior instances of this same shape (server pause → overlay card → resume verb) and SHOULD retrofit onto the registry later — the overlay slot, card chrome (`int-card` / `int-row` / `int-btn`), and interrupt plumbing are already shared; only their registration is bespoke today. Not retrofitted now (their wire events predate the pattern and are in production use).

### Session identity attach (identity_intake's host effect)

On an accepted `identity_intake` submit (either path) the server stamps the values onto the session the same way the pre-chat/create path does: metadata `userName` / `contactEmail` / `contactPhone` — exactly the keys the **OTP contact seam** reads (`AppState::session_contact`), so a captured contact is immediately OTP-verifiable. Kind host effects are routed by kind at one seam (`attach(kind, values)`); durable participant/CRM attach is a host concern (the monorepo's agent-brain rewires its `submit_contact_intake` CRM write onto it).

### Relationship to end-user OTP verification

Intake = **collect** (who are you?), OTP = **verify** (prove it). Shared machinery: park-and-resume interrupts, session-metadata contacts, the widget's overlay slot. Future unification (not built): a `verify: true` flag on the identity raise chaining into the existing `otp_verification_required` flow against the just-captured contact — collect-then-verify as one primitive.

## Channel matrix

| Channel | `supports` | identity_intake behavior |
| --- | --- | --- |
| Chat widget (web) | `["identity_form"]` (registry-derived) | parked inline form card; server-validated; decline button |
| SMS | — | conversational fallback: field-by-field ask, `submit_interaction` tool validates each value |
| Voice | — | same fallback (spoken turn-by-turn) |
| Future rich client | declares the kinds its cards cover | rich per declared kind, fallback for the rest |

## What changes where

| Layer | Change |
| ----- | ------ |
| `spec/` | `events/interaction-required.schema.json`, `events/interaction-invalid.schema.json`, `actions/submit-interaction.schema.json`, `spec/interactions/identity-intake.schema.json` (the kind catalog dir), `supports` on `create-conversation-session`, envelope enums, conformance fixtures |
| `rust/smooth-operator` | `interaction` module (the `InteractionKind` trait + `InteractionRegistry`); `identity_intake` module (validation + `IdentityIntakeKind`); `tools/interaction.rs` (generic raise wrapper + `submit_interaction` tool) |
| `rust/smooth-operator-server` | generic protocol constructors, `pending_interactions` registry + `session_capabilities`, `submit_interaction` dispatch, runner wiring (`TurnRequest::interactions`), kind-routed attach seam |
| `typescript/` (client) | regenerated types, `supports`, the single `submitInteraction()` resume verb |
| chat-widget repo | card registry + the identity card; declares `supports` from the registry |
| TS / Python / Go / .NET servers | **parity follow-ups** (per the repo's parity process; the spec here is the complete contract) |

## Design choices

- **No engine (`smooth-operator-core`) change.** The raise tools own their own request/outcome channel pair (the `ConfirmationHook` pattern) rather than extending `HumanRequest`. Unifying onto a structured `HumanRequest` variant is a possible later refactor.
- **Generic wire, per-kind tools.** See "Why a generic envelope" above: the wire is kind-agnostic (no client release per kind); LLM-facing precision lives in the per-kind raise tools; schema precision lives in `spec/interactions/`.
- **`interactionId` on the park**: a stale card submit can never resolve a newer park; duplicate submits are no-ops (`NO_PENDING_INTERACTION` / `INTERACTION_MISMATCH`).
- **Fail-open registration**: raise tools are always registered (the per-agent `enabled_tools` allow-list can restrict them); agents that never need an interaction never call one.
- This supersedes the short-lived typed `identity_intake_*` events shipped in 1.19.0 (same release train, zero external consumers — removed rather than deprecated).

---

**In this vault:** [[Protocol Reference]] · [[The Protocol]] · [[Agents, Tools, and Workflows]] · [[Polyglot Server Parity]]
