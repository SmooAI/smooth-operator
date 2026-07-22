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

// streamPreamble is one token of the optional fast-model preamble — an EPHEMERAL
// "what I'm about to do" line that the real answer replaces. Shaped identically to
// streamToken (token duplicated at the top level and under data) so clients can reuse
// the render path, but on a distinct type so it is never folded into the answer and
// never appears in eventual_response. Matches spec/events/stream-preamble.schema.json
// and the Rust protocol::stream_preamble.
func streamPreamble(requestID, token string) map[string]any {
	return map[string]any{
		"type":      "stream_preamble",
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

// writeConfirmationRequired is emitted mid-turn when the agent calls a
// state-mutating tool that requires explicit human approval before it runs. The
// turn is PARKED (the engine's HumanGate awaits the verdict) until the client
// replies with a confirm_tool_action action carrying the same requestId and an
// approved boolean.
//
// Wire shape matches spec/events/write-confirmation-required.schema.json and the
// Rust reference's write_confirmation_required byte-for-byte: the requestId echoes
// the originating send_message, and the prompt detail is double-nested under
// data.data.{toolId, actionDescription}. toolId is an opaque correlation handle
// (the tool name — a turn parks one tool at a time); actionDescription is the
// human-readable prompt the client renders.
func writeConfirmationRequired(requestID, toolID, actionDescription string) map[string]any {
	return map[string]any{
		"type":      "write_confirmation_required",
		"requestId": requestID,
		"data": map[string]any{
			"requestId": requestID,
			"data":      map[string]any{"toolId": toolID, "actionDescription": actionDescription},
		},
		"timestamp": nowMs(),
	}
}

// otpVerificationRequired is emitted after a turn's auth gate refused an end_user tool on
// an unverified session and the host has an OtpService installed. It tells the client to
// collect a one-time code. Wire shape matches spec/events/otp-verification-required.schema.json
// and the Rust reference (double-nested data.data). availableChannels are the delivery
// channels the server can offer given the session's known contacts ("email" / "sms").
func otpVerificationRequired(requestID, toolID, actionDescription string, availableChannels []OtpChannel, authLevel string) map[string]any {
	channels := make([]string, len(availableChannels))
	for i, c := range availableChannels {
		channels[i] = string(c)
	}
	return map[string]any{
		"type":      "otp_verification_required",
		"requestId": requestID,
		"data": map[string]any{
			"requestId": requestID,
			"data": map[string]any{
				"toolId":            toolID,
				"actionDescription": actionDescription,
				"availableChannels": channels,
				"authLevel":         authLevel,
			},
		},
		"timestamp": nowMs(),
	}
}

// otpSent acknowledges that a code was dispatched to the caller. Wire shape matches
// spec/events/otp-sent.schema.json. maskedDestination is a partially masked address safe to
// display (e.g. j***@example.com); the server never sees the code itself.
func otpSent(requestID string, channel OtpChannel, maskedDestination string) map[string]any {
	return map[string]any{
		"type":      "otp_sent",
		"requestId": requestID,
		"data": map[string]any{
			"requestId": requestID,
			"data": map[string]any{
				"channel":           string(channel),
				"maskedDestination": maskedDestination,
			},
		},
		"timestamp": nowMs(),
	}
}

// otpVerified is emitted when a verify_otp attempt succeeds. The session is now
// identity-verified; the client re-sends its message to run the gated tool (the reference
// server does not park/auto-resume the original turn). Wire shape matches
// spec/events/otp-verified.schema.json.
func otpVerified(requestID, message string) map[string]any {
	return map[string]any{
		"type":      "otp_verified",
		"requestId": requestID,
		"data": map[string]any{
			"requestId": requestID,
			"data":      map[string]any{"message": message},
		},
		"timestamp": nowMs(),
	}
}

// otpInvalid is emitted when a verify_otp attempt is rejected. errorCode is an optional
// machine-readable reason ("" ⇒ the key is omitted, per spec); attemptsRemaining of 0 means the
// code is locked and the client must restart the flow. Wire shape matches
// spec/events/otp-invalid.schema.json.
func otpInvalid(requestID string, errorCode OtpErrorCode, attemptsRemaining int, message string) map[string]any {
	inner := map[string]any{
		"attemptsRemaining": attemptsRemaining,
		"message":           message,
	}
	// Optional per spec: only emit `error` when the host determined a cause.
	if errorCode != "" {
		inner["error"] = string(errorCode)
	}
	return map[string]any{
		"type":      "otp_invalid",
		"requestId": requestID,
		"data": map[string]any{
			"requestId": requestID,
			"data":      inner,
		},
		"timestamp": nowMs(),
	}
}

// cancelled is the terminal event of a turn the client aborted with a `cancel` action —
// emitted IN PLACE OF the eventual_response a completed turn would send. It echoes the
// cancelled send_message's requestId so the client correlates the reset (drop the
// streaming indicator, re-enable input), at the envelope level AND inside `data` (the
// envelope convention). Status 499 mirrors nginx's "client closed request": a terminal,
// non-200 outcome distinct from a server error. There is NO answer payload — a cancelled
// turn produced no assistant message (the streamed tokens were ephemeral and are never
// persisted; the user's message stays persisted).
//
// Only built when a live turn was actually aborted; a cancel with no active turn is a
// no-op that emits nothing. Mirrors the Rust reference's protocol::cancelled.
func cancelled(requestID string) map[string]any {
	data := map[string]any{"status": 499}
	ev := map[string]any{
		"type":      "cancelled",
		"status":    499,
		"data":      data,
		"timestamp": nowMs(),
	}
	if requestID != "" {
		data["requestId"] = requestID
		ev["requestId"] = requestID
	}
	return ev
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
