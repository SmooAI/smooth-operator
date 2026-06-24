package server

import (
	"encoding/json"
	"path/filepath"
	"testing"

	"github.com/SmooAI/smooth-operator/go/protocol"
)

// specDir locates the repo's spec/ directory relative to this package (go/server).
func specDir(t *testing.T) string {
	t.Helper()
	dir, err := filepath.Abs(filepath.Join("..", "..", "spec"))
	if err != nil {
		t.Fatalf("resolve spec dir: %v", err)
	}
	return dir
}

// asTree round-trips a built event through JSON into the generic any tree the schema
// validator expects.
func asTree(t *testing.T, event map[string]any) any {
	t.Helper()
	raw, err := json.Marshal(event)
	if err != nil {
		t.Fatalf("marshal event: %v", err)
	}
	var v any
	if err := json.Unmarshal(raw, &v); err != nil {
		t.Fatalf("unmarshal event: %v", err)
	}
	return v
}

// TestEmittedEventsValidateAgainstSpec asserts every server→client event the server
// emits validates against the canonical spec/events/*.schema.json — the same schemas
// the Rust/C#/Go-client conformance suites check. This is the wire-contract guarantee.
func TestEmittedEventsValidateAgainstSpec(t *testing.T) {
	dir := specDir(t)
	v, err := protocol.NewValidator(dir)
	if err != nil {
		t.Fatalf("load validator: %v", err)
	}

	cases := []struct {
		name      string
		schemaRef string
		event     map[string]any
	}{
		{
			name:      "pong",
			schemaRef: "events/pong.schema.json",
			event:     pong("req-1"),
		},
		{
			name:      "immediate_response_create",
			schemaRef: "events/immediate-response.schema.json",
			event: immediateResponse("req-1", 200, "Session created", map[string]any{
				"sessionId": "s1", "conversationId": "c1",
			}),
		},
		{
			name:      "immediate_response_ack",
			schemaRef: "events/immediate-response.schema.json",
			event:     immediateResponse("req-2", 202, "Processing your request...", nil),
		},
		{
			name:      "stream_token",
			schemaRef: "events/stream-token.schema.json",
			event:     streamToken("req-2", "hello"),
		},
		{
			name:      "stream_chunk_tool_call",
			schemaRef: "events/stream-chunk.schema.json",
			event:     streamChunk("req-2", "echo", toolCallState("echo", `{"text":"hi"}`)),
		},
		{
			name:      "eventual_response",
			schemaRef: "events/eventual-response.schema.json",
			event:     eventualResponse("req-2", 200, "m1", generalResponse("hi there"), false, nil),
		},
		{
			name:      "eventual_response_with_citations",
			schemaRef: "events/eventual-response.schema.json",
			event: eventualResponse("req-3", 200, "m2", generalResponse("returns are 17 days"), false, []Citation{
				{ID: "doc-1", Title: "policies/returns.md", URL: "https://example.com/returns.md", Snippet: "17 days", Score: 0.9},
			}),
		},
		{
			name:      "error",
			schemaRef: "events/error.schema.json",
			event:     errorEvent("req-4", "NOT_FOUND", "Session not found"),
		},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			if err := v.ValidateRef(tc.schemaRef, asTree(t, tc.event)); err != nil {
				t.Fatalf("%s failed validation against %s: %v", tc.name, tc.schemaRef, err)
			}
		})
	}
}

// TestEmittedEventsRoundTripIntoClientTypes asserts the server's events parse cleanly
// through the Go client's protocol.ParseServerEvent and decode into the right concrete
// types — independently of the schema validator, this catches json-tag drift between
// the server's emitters and the client's reader.
func TestEmittedEventsRoundTripIntoClientTypes(t *testing.T) {
	marshal := func(t *testing.T, event map[string]any) []byte {
		t.Helper()
		raw, err := json.Marshal(event)
		if err != nil {
			t.Fatalf("marshal: %v", err)
		}
		return raw
	}

	t.Run("eventual_response", func(t *testing.T) {
		frame := marshal(t, eventualResponse("req-1", 200, "msg-123", generalResponse("hi"), false, nil))
		ev, err := protocol.ParseServerEvent(frame)
		if err != nil {
			t.Fatalf("parse: %v", err)
		}
		if ev.Type != protocol.EventEventualResponse {
			t.Fatalf("type = %q", ev.Type)
		}
		final, err := ev.AsEventualResponse()
		if err != nil {
			t.Fatalf("decode: %v", err)
		}
		if final.Data.Data.MessageID != "msg-123" {
			t.Fatalf("messageId = %q", final.Data.Data.MessageID)
		}
		if final.Data.Status != 200 {
			t.Fatalf("status = %d", final.Data.Status)
		}
	})

	t.Run("stream_token", func(t *testing.T) {
		frame := marshal(t, streamToken("req-1", "world"))
		ev, err := protocol.ParseServerEvent(frame)
		if err != nil {
			t.Fatalf("parse: %v", err)
		}
		if ev.Type != protocol.EventStreamToken || ev.Token != "world" {
			t.Fatalf("token round-trip mismatch: type=%q token=%q", ev.Type, ev.Token)
		}
	})

	t.Run("error", func(t *testing.T) {
		frame := marshal(t, errorEvent("req-1", "NOT_FOUND", "Session not found"))
		ev, err := protocol.ParseServerEvent(frame)
		if err != nil {
			t.Fatalf("parse: %v", err)
		}
		errEv, err := ev.AsError()
		if err != nil {
			t.Fatalf("decode error event: %v", err)
		}
		if errEv.Data.Error.Code != "NOT_FOUND" {
			t.Fatalf("error code = %q", errEv.Data.Error.Code)
		}
	})
}
