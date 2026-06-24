package server

import "time"

// Builders for the server→client protocol event frames. Every event is a
// map[string]any serialized as a JSON text frame. The shapes mirror the Rust
// reference server's protocol.rs and the C# ProtocolEvents.cs byte-for-byte
// (including the triple-nested eventual_response.data.data), so they validate
// against the same spec/events/*.schema.json and conformance fixtures and round-trip
// through the Go client's protocol.ParseServerEvent.

// Citation is one grounding source attached to a turn's terminal response.
type Citation struct {
	ID      string
	Title   string
	URL     string // empty → omitted
	Snippet string
	Score   float64
}

func nowMs() int64 { return time.Now().UnixMilli() }

// pong answers a ping. requestId is echoed when present.
func pong(requestID string) map[string]any {
	ev := map[string]any{"type": "pong", "timestamp": nowMs()}
	if requestID != "" {
		ev["requestId"] = requestID
	}
	return ev
}

// immediateResponse is the synchronous ack for an action (200 for create/get, 202
// for the send_message processing ack).
func immediateResponse(requestID string, status int, message string, data map[string]any) map[string]any {
	if data == nil {
		data = map[string]any{}
	}
	ev := map[string]any{
		"type":      "immediate_response",
		"status":    status,
		"message":   message,
		"data":      data,
		"timestamp": nowMs(),
	}
	if requestID != "" {
		ev["requestId"] = requestID
	}
	return ev
}

// streamToken is one incremental assistant text delta. Mirrors the C# StreamToken:
// the token is carried both at the top level and nested under data.
func streamToken(requestID, token string) map[string]any {
	return map[string]any{
		"type":      "stream_token",
		"requestId": requestID,
		"token":     token,
		"data":      map[string]any{"requestId": requestID, "token": token},
		"timestamp": nowMs(),
	}
}

// streamChunk is a workflow-node update (here: a tool call or a tool result),
// carrying an opaque state object under data.state.
func streamChunk(requestID, node string, state map[string]any) map[string]any {
	return map[string]any{
		"type":      "stream_chunk",
		"requestId": requestID,
		"node":      node,
		"data":      map[string]any{"requestId": requestID, "node": node, "state": state},
		"timestamp": nowMs(),
	}
}

// eventualResponse is the terminal turn event. Matches the Rust/C# shape: a
// triple-nested data.data carrying messageId, the agent response, needsEscalation,
// and (only when non-empty) the citations array.
func eventualResponse(requestID string, status int, messageID string, response map[string]any, needsEscalation bool, citations []Citation) map[string]any {
	inner := map[string]any{
		"messageId":       messageID,
		"response":        response,
		"needsEscalation": needsEscalation,
	}
	if len(citations) > 0 {
		arr := make([]map[string]any, 0, len(citations))
		for _, c := range citations {
			arr = append(arr, citationObject(c))
		}
		inner["citations"] = arr
	}
	return map[string]any{
		"type":      "eventual_response",
		"requestId": requestID,
		"status":    status,
		"data": map[string]any{
			"requestId": requestID,
			"status":    status,
			"data":      inner,
		},
		"timestamp": nowMs(),
	}
}

// errorEvent reports a handler/validation failure without dropping the connection.
//
// The {code, message} descriptor is duplicated at the envelope top level (`error`)
// AND nested under `data.error`, per spec/events/error.schema.json — the top-level
// copy is what clients and the conformance corpus pattern-match on; `data.error` is
// kept for wire backward-compatibility. Mirrors the Python/Rust/C#/TS servers.
func errorEvent(requestID, code, message string) map[string]any {
	descriptor := map[string]any{"code": code, "message": message}
	ev := map[string]any{
		"type":      "error",
		"error":     descriptor,
		"data":      map[string]any{"error": descriptor},
		"timestamp": nowMs(),
	}
	if requestID != "" {
		ev["requestId"] = requestID
	}
	return ev
}

// generalResponse wraps the agent's reply text in the minimal GeneralAgentResponse
// shape the protocol's response field expects.
func generalResponse(reply string) map[string]any {
	return map[string]any{
		"responseParts":          []string{reply},
		"customerHappinessScore": 0.5,
		"needsSatisfactionScore": 0.5,
		"requestSummary":         "",
		"resolutionStatus":       "in_progress",
		"suggestedNextActions":   []string{},
	}
}

func citationObject(c Citation) map[string]any {
	obj := map[string]any{
		"id":      c.ID,
		"title":   c.Title,
		"snippet": c.Snippet,
		"score":   c.Score,
	}
	if c.URL != "" {
		obj["url"] = c.URL
	}
	return obj
}
