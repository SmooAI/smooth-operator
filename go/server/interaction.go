package server

import (
	"context"
	"encoding/json"
	"sync"
)

// Rich Interactions — the extensible structured-interaction framework (the Go port
// of the Rust rust/smooth-operator/src/interaction.rs + smooth-operator-server
// wiring). One pattern, many kinds: an agent raises a structured interaction
// (choice chips, identity intake, a date picker, …) through a per-kind raise tool.
//
//   - On a channel whose client declared the kind's render capability (`supports`
//     at create_conversation_session), the raise PARKS the turn — the raise tool
//     blocks inside its Execute awaiting the visitor, the server emits
//     interaction_required, the client renders a rich card and replies with a
//     submit_interaction action, and the turn resumes with the validated payload.
//   - On a text-only channel the same raise degrades to the kind's conversational
//     fallback directive (the model collects the answer turn by turn).
//
// Both paths run the kind's server-side validator and resume with the SAME
// canonical payload. Adding a kind = implementing InteractionKind + registering it;
// no new protocol events, no new client verbs. This file is kind-agnostic; the
// reference kind is ChoicesKind (choices.go).
//
// Two registries live here, mirroring the confirmation HITL machinery
// (confirmation.go): InteractionKinds is the stateless catalog of hosted kinds,
// and InteractionRegistry is the per-connection park/resume map a submit_interaction
// frame resolves (the analog of ConfirmationRegistry).

// InteractionFieldError is a single per-field validation failure, carried on the
// interaction_invalid event. Field is a kind-specific key (choices: the question
// header).
type InteractionFieldError struct {
	Field   string `json:"field"`
	Message string `json:"message"`
}

// InteractionToolSchema is the LLM-facing schema for a kind's raise tool (per-kind,
// so parameters stay precise). Convention: Name is "request_<kind>".
type InteractionToolSchema struct {
	Name        string
	Description string
	Parameters  map[string]any
}

// InteractionRequest is a parsed raise: what the agent asked the visitor for.
type InteractionRequest struct {
	// Kind is the interaction kind (e.g. "choices").
	Kind string
	// Spec is the kind-specific render spec (shape per spec/interactions/<kind>.schema.json#/$defs/Spec).
	// Carried on interaction_required and re-parsed by Validate.
	Spec json.RawMessage
	// Reason is why the agent raised it (card header / woven into the conversational ask).
	Reason string
}

// InteractionOutcome is how a parked interaction resolved. Fed back to the parked
// raise tool by the submit_interaction handler (submitted/declined) or by teardown
// (no_response).
type InteractionOutcome struct {
	// Status is "submitted", "declined", or "no_response".
	Status string
	// Values is the validated, canonical payload — set only when Status == "submitted".
	Values any
}

const (
	outcomeSubmitted  = "submitted"
	outcomeDeclined   = "declined"
	outcomeNoResponse = "no_response"
)

// InteractionKind is one interaction kind — the extension seam. A kind supplies only
// what differs per interaction; all park/resume/event/registry machinery is shared:
//  1. identity (Kind / Capability),
//  2. the LLM-facing raise-tool surface (ToolSchema + ParseRequest),
//  3. the server-side validator (Validate) producing the canonical values,
//  4. the conversational degradation (FallbackDirective) for text-only channels.
type InteractionKind interface {
	// Kind is the wire kind id (e.g. "choices") — selects the client card and the validator.
	Kind() string
	// Capability is the client render capability that gates the rich path (e.g.
	// "choice_chips"). A session that declared it in `supports` parks the card;
	// anything else gets the conversational fallback.
	Capability() string
	// ToolSchema is the raise tool's LLM-facing schema (name it "request_<kind>").
	ToolSchema() InteractionToolSchema
	// ParseRequest parses + canonicalizes the raise tool's arguments into the kind's
	// spec (carried on interaction_required) and the human-readable reason. Errors on
	// malformed arguments; the text is surfaced to the model.
	ParseRequest(args map[string]any) (InteractionRequest, error)
	// Validate validates submitted values against spec, returning the canonical
	// (normalized) values or the full list of per-field errors (every failed field,
	// so a card can annotate all of them in one round-trip). A nil/empty spec ⇒
	// format-only validation (a prior-turn fallback raise whose spec is gone).
	Validate(spec json.RawMessage, values any) (any, []InteractionFieldError)
	// FallbackDirective is the conversational-degradation directive for text-only
	// channels: instructions the model follows to collect the same information turn
	// by turn.
	FallbackDirective(spec json.RawMessage, reason string) string
}

// InteractionEffect is the OPTIONAL host effect a kind runs after a valid submit — the
// framework's kind-agnostic effect seam (the Go analog of the Rust attach_interaction_effect
// switch). A kind with a host side effect (identity_intake: stamp the session identity)
// implements it; a kind without one (choices) simply doesn't, and attachInteractionEffect is
// a no-op for it. Fires on BOTH the rich submit_interaction path (dispatcher) and the
// conversational submit_interaction tool path (turn runner), with the canonical values the
// kind's Validate returned. Future kinds plug in by implementing this — no framework change.
type InteractionEffect interface {
	// AttachEffect applies the accepted interaction's host side effect for a session, given
	// the store to persist through and the canonical (validated) values. Best-effort: it must
	// not fail the already-produced turn (swallow persistence errors).
	AttachEffect(ctx context.Context, store SessionStore, sessionID string, values any)
}

// attachInteractionEffect runs kind's optional host effect (InteractionEffect) after a valid
// submit. Kind-agnostic: a kind without a host effect is a no-op. Called from both the rich
// (dispatcher) and conversational (turn runner) submit paths.
func attachInteractionEffect(ctx context.Context, store SessionStore, kind InteractionKind, sessionID string, values any) {
	if effect, ok := kind.(InteractionEffect); ok {
		effect.AttachEffect(ctx, store, sessionID, values)
	}
}

// InteractionKinds is the stateless catalog of interaction kinds a server hosts —
// the Go analog of the Rust InteractionRegistry-of-kinds. Immutable after build, so
// it needs no locking and can be shared across connections.
type InteractionKinds struct {
	kinds []InteractionKind
	byID  map[string]InteractionKind
}

// NewInteractionKinds builds a catalog from the given kinds (registration order
// preserved; a later kind with a duplicate id shadows an earlier one on lookup).
func NewInteractionKinds(kinds ...InteractionKind) *InteractionKinds {
	byID := make(map[string]InteractionKind, len(kinds))
	for _, k := range kinds {
		byID[k.Kind()] = k
	}
	return &InteractionKinds{kinds: kinds, byID: byID}
}

// DefaultInteractionKinds is the reference catalog: choices + identity_intake.
func DefaultInteractionKinds() *InteractionKinds {
	return NewInteractionKinds(ChoicesKind{}, IdentityIntakeKind{})
}

// Get looks up a kind by its wire id (nil if not hosted).
func (c *InteractionKinds) Get(kind string) InteractionKind {
	if c == nil {
		return nil
	}
	return c.byID[kind]
}

// All returns every registered kind, in registration order.
func (c *InteractionKinds) All() []InteractionKind {
	if c == nil {
		return nil
	}
	return c.kinds
}

// pendingInteraction is a parked raise awaiting a submit_interaction: the id the
// submit must echo, the kind (routes to its validator), the spec (drives
// validation), and the channel that resumes the parked raise tool.
type pendingInteraction struct {
	// InteractionID is the server-generated id for this instance; the submit must echo
	// it so a stale submit can never resolve a newer park.
	InteractionID string
	// Kind is the interaction kind (routes to its validator).
	Kind string
	// Spec is the kind-specific spec the raise carried (drives validation).
	Spec json.RawMessage
	// outcome resumes the parked raise tool. Buffered cap 1 so a resolve never blocks.
	outcome chan InteractionOutcome
}

// InteractionRegistry is the per-connection park/resume map for Rich Interactions —
// the kind-agnostic analog of ConfirmationRegistry. When a raise tool parks the turn
// it registers here (keyed by sessionId); a later submit_interaction frame on the
// same connection peeks the pending record to validate against its spec, then
// resolves it (feeding the outcome back to the parked raise tool). Touched from two
// goroutines — the parked turn registers + awaits, the read loop's submit handler
// resolves — so every method is mutex-guarded.
type InteractionRegistry struct {
	mu sync.Mutex
	// pending maps sessionId → the parked interaction awaiting submission. At most one
	// per session (a raise parks the turn; no second raise can run until it resumes).
	pending map[string]pendingInteraction
}

// NewInteractionRegistry builds an empty registry (no interaction parked).
func NewInteractionRegistry() *InteractionRegistry {
	return &InteractionRegistry{pending: map[string]pendingInteraction{}}
}

// Register registers (and returns) a fresh outcome channel for a parked raise on
// sessionID. Any prior pending interaction for the session is resolved no_response
// first, so a stale parked turn is never left dangling and the newest raise wins
// (mirrors ConfirmationRegistry.Register).
func (r *InteractionRegistry) Register(sessionID, interactionID, kind string, spec json.RawMessage) chan InteractionOutcome {
	r.mu.Lock()
	defer r.mu.Unlock()
	if prior, ok := r.pending[sessionID]; ok {
		select {
		case prior.outcome <- InteractionOutcome{Status: outcomeNoResponse}:
		default:
		}
		delete(r.pending, sessionID)
	}
	ch := make(chan InteractionOutcome, 1)
	r.pending[sessionID] = pendingInteraction{InteractionID: interactionID, Kind: kind, Spec: spec, outcome: ch}
	return ch
}

// Pending peeks the parked interaction for sessionID WITHOUT consuming it — the
// submit handler needs the kind + spec to validate before deciding whether to
// resolve (an invalid submit must leave the turn parked for a resubmit).
func (r *InteractionRegistry) Pending(sessionID string) (pendingInteraction, bool) {
	r.mu.Lock()
	defer r.mu.Unlock()
	p, ok := r.pending[sessionID]
	return p, ok
}

// Resolve resolves the parked raise for sessionID with the outcome, IF interactionID
// echoes the pending instance. Returns false when nothing is parked or the id
// mismatches (a stale/duplicate submit → a clean no-op). Taking the record out makes
// a duplicate resolve a no-op (mirrors the Rust take-on-resolution).
func (r *InteractionRegistry) Resolve(sessionID, interactionID string, outcome InteractionOutcome) bool {
	r.mu.Lock()
	defer r.mu.Unlock()
	p, ok := r.pending[sessionID]
	if !ok || p.InteractionID != interactionID {
		return false
	}
	delete(r.pending, sessionID)
	// Buffered cap 1 → never blocks; the parked raise tool receives the outcome.
	p.outcome <- outcome
	return true
}

// Clear drops any parked interaction for sessionID (turn ended) so a stale entry
// can't mis-route a later submit. Idempotent.
func (r *InteractionRegistry) Clear(sessionID string) {
	r.mu.Lock()
	defer r.mu.Unlock()
	delete(r.pending, sessionID)
}

// RejectAll resolves every outstanding parked interaction as no_response — called on
// connection teardown so any turn parked on a raise unparks and finishes cleanly
// (never leave a turn hung forever). Mirrors ConfirmationRegistry.RejectAll.
func (r *InteractionRegistry) RejectAll() {
	r.mu.Lock()
	defer r.mu.Unlock()
	for sid, p := range r.pending {
		select {
		case p.outcome <- InteractionOutcome{Status: outcomeNoResponse}:
		default:
		}
		delete(r.pending, sid)
	}
}
