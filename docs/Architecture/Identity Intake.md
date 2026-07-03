# Identity Intake — channel-normalized lead/identity capture

**Status: Accepted** (design + Rust reference implementation in this repo; parity servers tracked as follow-ups). This doc is the design record (the repo has no numbered-ADR convention; Architecture docs serve that role).

## Problem

Agents constantly need the visitor's **name / email / phone** — to create a CRM contact, send a follow-up, or verify identity later. Today each channel improvises:

- The chat widget has a **pre-chat form** (all-or-nothing, before the conversation starts).
- On SMS/voice the model **freestyles** the asks, with no server-side validation — typos in emails and unparseable phone numbers land in the CRM.
- The monorepo's `submit_contact_intake` tool trusts whatever the model extracted.

There is no **channel-normalized primitive**: one agent-visible verb that, on a form-capable channel, renders a structured inline form and, on a text-only channel, degrades to validated turn-by-turn conversational asks — resuming the turn with the **same validated structured payload** either way.

## Decision

Add an **identity-intake seam** to the protocol + reference server, shaped exactly like the existing HITL interrupts (`write_confirmation_required` / `otp_verification_required`):

1. **Capability negotiation** — the client declares what it can render at session create:

   ```jsonc
   { "action": "create_conversation_session", "agentId": "…", "supports": ["identity_form"] }
   ```

   `supports` is an optional array of client render capabilities. The chat widget declares `identity_form`; SMS/voice channels never do. Unknown values are ignored (forward-compatible).

2. **Agent tool `request_identity_intake`** — the agent raises the need:

   ```jsonc
   { "fields": [ { "key": "email", "required": true, "label": "Work email" },
                 { "key": "name",  "required": false } ],
     "reason": "to send you the quote" }
   ```

   Field keys are the closed set `name | email | phone`.

3. **Form path** (session declared `identity_form`): the turn **parks** inside the tool (same park-and-resume machinery as the write-confirmation hook) and the server emits:

   ```jsonc
   { "type": "identity_intake_required", "requestId": "…",
     "data": { "requestId": "…", "data": { "fields": [ … ], "reason": "…" } } }
   ```

   The client renders the inline form and replies with the resume action:

   ```jsonc
   { "action": "submit_identity_intake", "sessionId": "…", "requestId": "…",
     "values": { "email": "a@b.com", "name": "Alice" } }
   ```

   or declines: `{ …, "declined": true }`. The server **validates server-side** (required fields present, email format, phone → E.164 normalization). Invalid values emit `identity_intake_invalid` (mirroring `otp_invalid` — the turn **stays parked**, the form re-renders with per-field errors):

   ```jsonc
   { "type": "identity_intake_invalid", "requestId": "…",
     "data": { "requestId": "…", "data": {
       "errors": [ { "field": "email", "message": "must be a valid email address" } ],
       "message": "Some fields need attention." } } }
   ```

   Valid values resume the parked tool, which returns the structured payload to the model:

   ```jsonc
   { "status": "submitted", "values": { "email": "a@b.com", "name": "Alice" } }
   // or
   { "status": "declined" }
   ```

4. **Conversational fallback** (no `identity_form` capability): `request_identity_intake` does **not** park. It immediately returns a directive instructing the model to collect the fields **one at a time, conversationally**, and to call the companion tool `submit_identity_intake(name?, email?, phone?)` once collected. That tool runs the **same server-side validation**; a bad value returns a per-field tool error the model relays ("that email doesn't look right — could you re-check it?") and re-asks. On success it returns the **identical** `{ "status": "submitted", "values": … }` payload. The agent's flow downstream of the intake is therefore channel-independent.

5. **Session identity attach** — on a successful submit (either path) the server stamps the values onto the session the same way the pre-chat/create path does: session metadata `userName` / `contactEmail` / `contactPhone`. `contactEmail`/`contactPhone` are exactly the keys the **OTP contact seam** reads (`AppState::session_contact`), so a captured contact immediately becomes OTP-verifiable. Durable participant/CRM attach is a host concern (the monorepo's agent-brain rewires its `submit_contact_intake` CRM write onto this seam).

## Relationship to end-user OTP verification

- **Intake = collect** (who are you?), **OTP = verify** (prove it). They share machinery: park-and-resume interrupts, session-metadata contacts, the widget's above-composer interrupt overlay.
- Future unification (not built now): a `verify: true` flag on `request_identity_intake` that chains straight into the existing `otp_verification_required` flow against the just-captured contact — collect-then-verify as one primitive. The shared `contactEmail`/`contactPhone` keys are the designed-in seam for that.

## Validation rules (server-side, both paths)

| field | rule | normalization |
| ----- | ---- | ------------- |
| `name` | non-empty after trim | trimmed |
| `email` | `local@domain.tld` shape (single `@`, dot in domain, no whitespace) | trimmed, lowercased domain |
| `phone` | E.164 after stripping separators: `+` + 8–15 digits; bare 10-digit / 1-prefixed 11-digit NANP accepted | normalized to `+…` E.164 |

Validation lives in `smooth_operator::identity_intake` (the system crate) and is shared by the WS handler (form path) and the `submit_identity_intake` tool (conversational path) — one implementation, one behavior.

## What changes where

| Layer | Change |
| ----- | ------ |
| `spec/` | `events/identity-intake-required.schema.json`, `events/identity-intake-invalid.schema.json`, `actions/submit-identity-intake.schema.json`, `supports` on `create-conversation-session`, envelope enums, conformance fixtures |
| `rust/smooth-operator` | `identity_intake` module: field/values types + shared validation; `tools/identity_intake.rs`: `RequestIdentityIntakeTool` (park-or-directive) + `SubmitIdentityIntakeTool` (validate + attach) |
| `rust/smooth-operator-server` | protocol constructors, `pending_intakes` registry + `attach_session_identity` on `AppState`, `supports` parsing at create-session, `submit_identity_intake` dispatch, runner wiring (`TurnRequest::identity_intake`) |
| `typescript/` (client) | regenerated types, `supports` on `createConversationSession`, `submitIdentityIntake()` resume verb |
| chat-widget repo | declares `identity_form`, renders the interrupt form above the composer (pre-chat-form field pattern + OTP overlay pattern), submit/decline |
| TS / Python / Go / .NET servers | **parity follow-ups** (tracked per the repo's parity process; the spec here is the complete contract) |

## Design choices

- **No engine (`smooth-operator-core`) change.** The intake tool owns its own request/outcome channel pair (the `ConfirmationHook` pattern) rather than extending `HumanRequest`. Core's `HumanRequest::Input` stays untouched; unifying intake onto a structured `HumanRequest` variant is a possible later refactor once a second structured interrupt exists.
- **Two tools, one verb name on the wire.** The resume *action* (`submit_identity_intake`) and the model-callable *tool* (`submit_identity_intake`) intentionally share a name: same validation, same payload, same session attach — the channel only changes who fills the form (the visitor's fingers vs. the model's turn-by-turn collection).
- **`identity_intake_invalid` instead of an `error` event** for bad form values: `error` is terminal for a client turn (the widget's `MessageTurn` aborts on it); invalid input is a *retryable* state, exactly like `otp_invalid`.
- **Fail-open registration**: the tools are always registered (the per-agent `enabled_tools` allow-list can restrict them); agents that never mention intake never call them.

---

**In this vault:** [[Protocol Reference]] · [[The Protocol]] · [[Agents, Tools, and Workflows]] · [[Polyglot Server Parity]]
