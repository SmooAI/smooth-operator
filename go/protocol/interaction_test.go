package protocol

import (
	"context"
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// TestEventTypesCoverSpec is the drift guard. It derives the expected discriminator
// set from spec/events/*.schema.json — the source of truth — rather than from a list
// written here, so adding an event schema without wiring it into eventTypes fails
// this test instead of being silently dropped by ParseServerEvent at runtime. A
// guard asserting against a constant maintained alongside the one it guards would
// just lock the drift in.
func TestEventTypesCoverSpec(t *testing.T) {
	specEvents := specEventDiscriminators(t)
	if len(specEvents) == 0 {
		t.Fatal("no event schemas discovered in spec/events")
	}
	for _, disc := range specEvents {
		if !IsKnownEventType(EventType(disc)) {
			t.Errorf("spec/events declares event %q but eventTypes omits it: "+
				"ParseServerEvent will reject the frame and the dispatch loop will drop it silently", disc)
		}
	}
}

// TestActionTypesCoverSpec is the same guard for the client→server direction: the
// ActionType constants had drifted too (submit_interaction was missing), which is
// why the verb could not be expressed at all.
func TestActionTypesCoverSpec(t *testing.T) {
	known := map[string]struct{}{
		string(ActionCreateConversationSession): {},
		string(ActionSendMessage):               {},
		string(ActionGetSession):                {},
		string(ActionGetConversationMessages):   {},
		string(ActionConfirmToolAction):         {},
		string(ActionVerifyOTP):                 {},
		string(ActionSubmitInteraction):         {},
		string(ActionCancel):                    {},
		string(ActionPing):                      {},
	}

	specActions := specActionDiscriminators(t)
	if len(specActions) == 0 {
		t.Fatal("no action schemas discovered in spec/actions")
	}
	for _, disc := range specActions {
		if _, ok := known[disc]; !ok {
			t.Errorf("spec/actions declares action %q but the ActionType constants omit it", disc)
		}
	}
}

// specActionDiscriminators reads every spec/actions/*.schema.json and returns the
// `const` value of the `action` property on its Request definition.
func specActionDiscriminators(t *testing.T) []string {
	t.Helper()
	dir := filepath.Join(specDir(t), "actions")
	entries, err := os.ReadDir(dir)
	if err != nil {
		t.Fatalf("read spec/actions: %v", err)
	}
	var out []string
	for _, e := range entries {
		if e.IsDir() || !strings.HasSuffix(e.Name(), ".schema.json") {
			continue
		}
		raw, err := os.ReadFile(filepath.Join(dir, e.Name()))
		if err != nil {
			t.Fatalf("read %s: %v", e.Name(), err)
		}
		// Action schemas nest the frame under $defs/Request.
		var schema struct {
			Defs struct {
				Request struct {
					Properties struct {
						Action struct {
							Const string `json:"const"`
						} `json:"action"`
					} `json:"properties"`
				} `json:"Request"`
			} `json:"$defs"`
		}
		if err := json.Unmarshal(raw, &schema); err != nil {
			t.Fatalf("parse %s: %v", e.Name(), err)
		}
		if c := schema.Defs.Request.Properties.Action.Const; c != "" {
			out = append(out, c)
		}
	}
	return out
}

// specEventDiscriminators reads every spec/events/*.schema.json and returns the
// `const` value of its `type` property.
func specEventDiscriminators(t *testing.T) []string {
	t.Helper()
	dir := filepath.Join(specDir(t), "events")
	entries, err := os.ReadDir(dir)
	if err != nil {
		t.Fatalf("read spec/events: %v", err)
	}
	var out []string
	for _, e := range entries {
		if e.IsDir() || !strings.HasSuffix(e.Name(), ".schema.json") {
			continue
		}
		raw, err := os.ReadFile(filepath.Join(dir, e.Name()))
		if err != nil {
			t.Fatalf("read %s: %v", e.Name(), err)
		}
		var schema struct {
			Properties struct {
				Type struct {
					Const string `json:"const"`
				} `json:"type"`
			} `json:"properties"`
		}
		if err := json.Unmarshal(raw, &schema); err != nil {
			t.Fatalf("parse %s: %v", e.Name(), err)
		}
		if c := schema.Properties.Type.Const; c != "" {
			out = append(out, c)
		}
	}
	return out
}

// retarget rewrites a fixture instance's requestId (top-level and any nested data
// envelopes) so it correlates with the turn under test.
func retarget(t *testing.T, raw json.RawMessage, requestID string) map[string]any {
	t.Helper()
	var m map[string]any
	if err := json.Unmarshal(raw, &m); err != nil {
		t.Fatalf("decode fixture: %v", err)
	}
	var walk func(v any)
	walk = func(v any) {
		obj, ok := v.(map[string]any)
		if !ok {
			return
		}
		if _, present := obj["requestId"]; present {
			obj["requestId"] = requestID
		}
		for _, child := range obj {
			walk(child)
		}
	}
	walk(m)
	return m
}

// drain collects every event the turn surfaces, returning once the terminal event
// closes the channel. No timer: the terminal eventual_response always decodes (it
// was never part of this bug), so the drain always completes and a dropped frame
// shows up as an absent entry in the returned slice rather than as a timeout. That
// keeps the assertion deterministic on a loaded machine.
func drain(turn *MessageTurn) []ServerEvent {
	var out []ServerEvent
	for ev := range turn.Events() {
		out = append(out, ev)
	}
	return out
}

// types maps events to their discriminators, for readable assertions.
func types(evs []ServerEvent) []EventType {
	out := make([]EventType, len(evs))
	for i, ev := range evs {
		out[i] = ev.Type
	}
	return out
}

// find returns the first event of the given type, or fails naming what did arrive.
func find(t *testing.T, evs []ServerEvent, want EventType) ServerEvent {
	t.Helper()
	for _, ev := range evs {
		if ev.Type == want {
			return ev
		}
	}
	t.Fatalf("%s was dropped by the dispatch loop; got %v", want, types(evs))
	return ServerEvent{}
}

// terminalFrame is a minimal eventual_response that settles a turn and closes its
// event channel.
func terminalFrame(requestID string) map[string]any {
	return map[string]any{
		"type": "eventual_response", "requestId": requestID, "status": 200,
		"data": map[string]any{
			"requestId": requestID, "status": 200,
			"data": map[string]any{
				"messageId": "66666666-6666-6666-6666-666666666666",
				"response":  map[string]any{"responseParts": []any{"done"}},
			},
		},
	}
}

// TestInteractionFixturesReachTheTurn pushes the real spec fixtures through the
// client's actual dispatch path (transport → dispatchLoop → ParseServerEvent →
// turn). The existing conformance tests validate these same fixtures against the
// schemas but never feed them to the dispatcher, which is exactly the blind spot
// that let interaction_required/interaction_invalid be dropped at runtime.
func TestInteractionFixturesReachTheTurn(t *testing.T) {
	fixtures := loadFixtures(t, specDir(t))
	c, tr := makeClient(t)
	defer c.Close()

	turn := c.SendMessage(SendMessageParams{SessionID: "sess-1", Message: "quote please"})
	reqID := turn.RequestID()

	// The park, the validation rejection (the turn stays parked, so it must arrive
	// too), then the terminal that settles the turn and closes the channel.
	tr.emit(t, retarget(t, fixtures["interaction_required_event"].Instance, reqID))
	tr.emit(t, retarget(t, fixtures["interaction_invalid_event"].Instance, reqID))
	tr.emit(t, terminalFrame(reqID))

	evs := drain(turn)

	park := find(t, evs, EventInteractionRequired)
	req, err := park.AsInteractionRequired()
	if err != nil {
		t.Fatalf("AsInteractionRequired: %v", err)
	}
	if req.Data.Data.Kind != "identity_intake" {
		t.Errorf("kind = %q, want identity_intake", req.Data.Data.Kind)
	}
	interactionID := req.Data.Data.InteractionID
	if interactionID != "88888888-8888-8888-8888-888888888888" {
		t.Errorf("interactionId = %q", interactionID)
	}

	invalidEv := find(t, evs, EventInteractionInvalid)
	inv, err := invalidEv.AsInteractionInvalid()
	if err != nil {
		t.Fatalf("AsInteractionInvalid: %v", err)
	}
	if len(inv.Data.Data.Errors) != 1 || inv.Data.Data.Errors[0].Field != "email" {
		t.Errorf("errors = %+v, want one error on field email", inv.Data.Data.Errors)
	}
	if invalidEv.IsTerminal() {
		t.Error("interaction_invalid must not be terminal — the turn stays parked for a resubmit")
	}

	// Answering the park produces a spec-valid frame.
	if err := c.SubmitInteraction(SubmitInteractionParams{
		SessionID:     "22222222-2222-2222-2222-222222222222",
		RequestID:     reqID,
		InteractionID: interactionID,
		Kind:          "identity_intake",
		Values:        map[string]any{"name": "Alice Example", "email": "alice@example.com"},
	}); err != nil {
		t.Fatalf("SubmitInteraction: %v", err)
	}
	sent := tr.lastSent(t)
	if sent["action"] != string(ActionSubmitInteraction) {
		t.Fatalf("action = %v, want submit_interaction", sent["action"])
	}
	if sent["interactionId"] != interactionID {
		t.Errorf("frame interactionId = %v", sent["interactionId"])
	}
	if _, declinedPresent := sent["declined"]; declinedPresent {
		t.Error("declined must stay off the wire when not declining")
	}
	validateAgainstSpec(t, "actions/submit-interaction.schema.json#/$defs/Request", sent)
}

// TestSubmitInteractionDeclined covers the decline half of the values-or-declined
// wire shape against its own fixture's schema.
func TestSubmitInteractionDeclined(t *testing.T) {
	c, tr := makeClient(t)
	defer c.Close()

	if err := c.SubmitInteraction(SubmitInteractionParams{
		SessionID:     "22222222-2222-2222-2222-222222222222",
		RequestID:     "req-a1b2c3d4-0004",
		InteractionID: "88888888-8888-8888-8888-888888888888",
		Declined:      true,
	}); err != nil {
		t.Fatalf("SubmitInteraction: %v", err)
	}
	sent := tr.lastSent(t)
	if sent["declined"] != true {
		t.Fatalf("declined = %v, want true", sent["declined"])
	}
	if _, ok := sent["values"]; ok {
		t.Error("values must be omitted when declining")
	}
	validateAgainstSpec(t, "actions/submit-interaction.schema.json#/$defs/Request", sent)
}

// TestChoicesValuesSubmitRoundTrip asserts the ONE submit verb carries a second
// interaction kind's values unchanged — the choices kind needs no new client method.
func TestChoicesValuesSubmitRoundTrip(t *testing.T) {
	fixtures := loadFixtures(t, specDir(t))
	var values map[string]any
	if err := json.Unmarshal(fixtures["choices_values"].Instance, &values); err != nil {
		t.Fatalf("decode choices_values: %v", err)
	}

	c, tr := makeClient(t)
	defer c.Close()

	if err := c.SubmitInteraction(SubmitInteractionParams{
		SessionID:     "22222222-2222-2222-2222-222222222222",
		RequestID:     "req-a1b2c3d4-0004",
		InteractionID: "88888888-8888-8888-8888-888888888888",
		Kind:          "choices",
		Values:        values,
	}); err != nil {
		t.Fatalf("SubmitInteraction: %v", err)
	}
	sent := tr.lastSent(t)
	got, err := json.Marshal(sent["values"])
	if err != nil {
		t.Fatalf("re-marshal values: %v", err)
	}
	want, _ := json.Marshal(values)
	if string(got) != string(want) {
		t.Errorf("values round-trip lost data:\n got %s\nwant %s", got, want)
	}
	validateAgainstSpec(t, "actions/submit-interaction.schema.json#/$defs/Request", sent)
}

// TestEphemeralStreamEventsReachTheTurn covers stream_preamble and stream_reasoning.
// Both are emitted by the production Rust server today and have no conformance
// fixture, so the frames are built here and validated against their own schemas
// before being dispatched — a frame the spec would reject proves nothing.
func TestEphemeralStreamEventsReachTheTurn(t *testing.T) {
	c, tr := makeClient(t)
	defer c.Close()

	turn := c.SendMessage(SendMessageParams{SessionID: "sess-1", Message: "think about it"})
	reqID := turn.RequestID()

	cases := []struct {
		name   string
		schema string
		want   EventType
		frame  map[string]any
	}{
		{
			name:   "stream_preamble",
			schema: "events/stream-preamble.schema.json",
			want:   EventStreamPreamble,
			frame: map[string]any{
				"type": "stream_preamble", "requestId": reqID, "token": "Looking that up…",
				"data": map[string]any{"requestId": reqID, "token": "Looking that up…"},
			},
		},
		{
			name:   "stream_reasoning",
			schema: "events/stream-reasoning.schema.json",
			want:   EventStreamReasoning,
			frame: map[string]any{
				"type": "stream_reasoning", "requestId": reqID, "token": "let me think",
				"data": map[string]any{"requestId": reqID, "token": "let me think"},
			},
		},
	}

	for _, tc := range cases {
		validateAgainstSpec(t, tc.schema, tc.frame)
		tr.emit(t, tc.frame)
	}
	tr.emit(t, terminalFrame(reqID))

	evs := drain(turn)

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			ev := find(t, evs, tc.want)
			if ev.Token == "" {
				t.Error("envelope Token not populated")
			}
			if ev.IsTerminal() {
				t.Errorf("%s must not be terminal", tc.name)
			}
		})
	}
}

// TestUnknownEventIsIgnoredNotFatal is the OTHER half of the contract, and the
// reason the drift guard must not be "fixed" by making unknown types an error:
// the stream_reasoning schema says clients that do not recognize an event MUST
// ignore it. A future server sending a frame this client version predates has to
// be dropped silently, leaving the turn healthy and still able to complete.
func TestUnknownEventIsIgnoredNotFatal(t *testing.T) {
	c, tr := makeClient(t)
	defer c.Close()

	turn := c.SendMessage(SendMessageParams{SessionID: "sess-1", Message: "hi"})
	reqID := turn.RequestID()

	// A plausible frame from a NEWER server, on a type this build has never heard of.
	tr.emit(t, map[string]any{
		"type": "stream_hologram", "requestId": reqID, "token": "from the future",
		"data": map[string]any{"requestId": reqID, "token": "from the future"},
	})
	tr.emit(t, map[string]any{
		"type": "stream_token", "requestId": reqID, "token": "real",
		"data": map[string]any{"requestId": reqID, "token": "real"},
	})
	tr.emit(t, terminalFrame(reqID))

	evs := drain(turn)

	for _, ev := range evs {
		if ev.Type == "stream_hologram" {
			t.Error("an unrecognised event type must not be surfaced to consumers")
		}
	}
	// The unknown frame must not have derailed the turn: the known events still land.
	find(t, evs, EventStreamToken)
	find(t, evs, EventEventualResponse)

	if _, err := turn.Wait(context.Background()); err != nil {
		t.Errorf("turn must settle normally despite an unknown frame, got %v", err)
	}

	// And the parser reports it as unknown rather than as malformed JSON.
	_, err := ParseServerEvent([]byte(`{"type":"stream_hologram","data":{}}`))
	var unknown *UnknownEventError
	if !errors.As(err, &unknown) {
		t.Fatalf("ParseServerEvent error = %v, want *UnknownEventError", err)
	}
	if unknown.Type != "stream_hologram" {
		t.Errorf("UnknownEventError.Type = %q", unknown.Type)
	}
}

// validateAgainstSpec checks an instance against a spec schema ref, so a test that
// asserts on a frame also proves the frame is one the protocol actually allows.
func validateAgainstSpec(t *testing.T, ref string, instance any) {
	t.Helper()
	v, err := NewValidator(specDir(t))
	if err != nil {
		t.Fatalf("load validator: %v", err)
	}
	if err := v.ValidateRef(ref, instance); err != nil {
		t.Fatalf("frame failed validation against %s: %v", ref, err)
	}
}
