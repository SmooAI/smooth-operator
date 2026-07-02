package server

import (
	"context"
	"testing"

	core "github.com/SmooAI/smooth-operator-core/go/core"
	"github.com/SmooAI/smooth-operator/go/protocol"
)

// otpTool is the end_user-gated tool the offer-flow e2e drives; it records whether its real
// Fn ran (it must NOT until the session is verified).
func otpTool(name string, ran *bool) core.Tool {
	return core.FuncTool{
		ToolName: name,
		Desc:     "Look up the user's orders.",
		Params:   map[string]any{"type": "object"},
		Fn: func(_ context.Context, _ map[string]any) (string, error) {
			*ran = true
			return "ORDERS: #42", nil
		},
	}
}

// spawnOtpServer stands up a local WS server with an end_user-gated tool, a per-agent config,
// and the given OtpService — the shared rig for the offer-flow / verify e2e tests.
func spawnOtpServer(t *testing.T, store SessionStore, mock core.ChatClient, tool core.Tool, cfg *AgentConfig, svc OtpService) *LocalServer {
	t.Helper()
	opts := []LocalOption{
		WithLocalAddr("127.0.0.1:0"),
		WithLocalChatClient(mock),
		WithLocalServerOption(WithSessionStore(store)),
		WithLocalServerOption(WithTools([]core.Tool{tool})),
		WithLocalServerOption(WithAuthRequiringTools(tool.Name())),
		WithLocalServerOption(WithAgentConfigResolver(NewStaticAgentConfigResolver(map[string]*AgentConfig{e2eAgentID: cfg}))),
		WithLocalServerOption(WithOtpService(svc)),
	}
	ls, err := SpawnLocal(opts...)
	if err != nil {
		t.Fatalf("spawn: %v", err)
	}
	return ls
}

// collectUntilEventual reads events until (and including) eventual_response, returning the
// ordered type list and the last-seen otp_verification_required / otp_sent events.
func collectUntilEventual(t *testing.T, transport protocol.Transport) (order []string, required, sent map[string]any) {
	t.Helper()
	for {
		ev := nextEv(t, transport)
		typ, _ := ev["type"].(string)
		order = append(order, typ)
		switch typ {
		case "otp_verification_required":
			required = ev
		case "otp_sent":
			sent = ev
		case "eventual_response":
			return order, required, sent
		}
	}
}

func TestOtpOfferFlowE2E(t *testing.T) {
	const toolName = "lookup_orders"

	t.Run("end_user refusal offers OTP: prompt → sent → eventual_response", func(t *testing.T) {
		var ran bool
		mock := core.NewMockLlmProvider()
		mock.PushToolCall("call-1", toolName, `{}`)
		mock.PushText("Let me verify you first.")

		svc := &fakeOtp{delivery: OtpDelivery{Channel: OtpChannelEmail, MaskedDestination: "a***@example.com"}}
		cfg := &AgentConfig{Visibility: "public", EnabledTools: []EnabledTool{{ToolID: toolName, Enabled: true, AuthLevel: "end_user"}}}
		ls := spawnOtpServer(t, NewInMemorySessionStore(), mock, otpTool(toolName, &ran), cfg, svc)
		defer ls.Shutdown()

		transport := connectTransport(t, ls)
		defer transport.Close()
		sessionID := createSession(t, transport)

		sendFrame(t, transport, map[string]any{"action": "send_message", "requestId": "r-msg", "sessionId": sessionID, "message": "show my orders"})

		order, required, sent := collectUntilEventual(t, transport)

		if ran {
			t.Error("the end_user tool must NOT run before verification")
		}
		// Order: otp_verification_required precedes otp_sent, both precede eventual_response.
		ri, si, ei := typeIndex(order, "otp_verification_required"), typeIndex(order, "otp_sent"), typeIndex(order, "eventual_response")
		if ri < 0 || si < 0 || !(ri < si && si < ei) {
			t.Fatalf("bad event order %v (required=%d sent=%d eventual=%d)", order, ri, si, ei)
		}
		// otp_verification_required payload names the refused tool, offers email, level end_user.
		inner := dataData(t, required)
		if inner["toolId"] != toolName || inner["authLevel"] != "end_user" {
			t.Errorf("otp_verification_required inner = %+v", inner)
		}
		chans, _ := inner["availableChannels"].([]any)
		if len(chans) != 1 || chans[0] != "email" {
			t.Errorf("availableChannels = %+v, want [email]", inner["availableChannels"])
		}
		// otp_sent carries the host's channel + masked destination.
		si2 := dataData(t, sent)
		if si2["channel"] != "email" || si2["maskedDestination"] != "a***@example.com" {
			t.Errorf("otp_sent inner = %+v", si2)
		}
		// The host's SendOtp saw the session's captured contact email.
		if svc.sentContact.Email != "alice@example.com" {
			t.Errorf("SendOtp contact = %+v, want alice@example.com", svc.sentContact)
		}
	})

	t.Run("admin refusal is NOT offered OTP", func(t *testing.T) {
		var ran bool
		mock := core.NewMockLlmProvider()
		mock.PushToolCall("call-1", toolName, `{}`)
		mock.PushText("That needs staff access.")

		svc := &fakeOtp{delivery: OtpDelivery{Channel: OtpChannelEmail, MaskedDestination: "a***@example.com"}}
		cfg := &AgentConfig{Visibility: "public", EnabledTools: []EnabledTool{{ToolID: toolName, Enabled: true, AuthLevel: "admin"}}}
		ls := spawnOtpServer(t, NewInMemorySessionStore(), mock, otpTool(toolName, &ran), cfg, svc)
		defer ls.Shutdown()

		transport := connectTransport(t, ls)
		defer transport.Close()
		sessionID := createSession(t, transport)

		sendFrame(t, transport, map[string]any{"action": "send_message", "requestId": "r-msg", "sessionId": sessionID, "message": "run the admin tool"})

		order, required, _ := collectUntilEventual(t, transport)
		if required != nil {
			t.Errorf("admin refusal must NOT offer OTP, got order %v", order)
		}
		if svc.sentContact.Email != "" {
			t.Error("SendOtp must not be called for an admin refusal")
		}
	})

	t.Run("verified session runs the end_user tool on re-send", func(t *testing.T) {
		var ran bool
		mock := core.NewMockLlmProvider()
		// Turn 1: refused end_user tool + wrap-up.
		mock.PushToolCall("call-1", toolName, `{}`)
		mock.PushText("Verify first, please.")
		// Turn 2 (after verify): the tool runs, then a wrap-up.
		mock.PushToolCall("call-2", toolName, `{}`)
		mock.PushText("Here are your orders.")

		svc := &fakeOtp{delivery: OtpDelivery{Channel: OtpChannelEmail, MaskedDestination: "a***@example.com"}, outcome: Verified()}
		cfg := &AgentConfig{Visibility: "public", EnabledTools: []EnabledTool{{ToolID: toolName, Enabled: true, AuthLevel: "end_user"}}}
		store := NewInMemorySessionStore()
		ls := spawnOtpServer(t, store, mock, otpTool(toolName, &ran), cfg, svc)
		defer ls.Shutdown()

		transport := connectTransport(t, ls)
		defer transport.Close()
		sessionID := createSession(t, transport)

		// Turn 1: refused → offered.
		sendFrame(t, transport, map[string]any{"action": "send_message", "requestId": "r-1", "sessionId": sessionID, "message": "show my orders"})
		if _, required, _ := collectUntilEventual(t, transport); required == nil {
			t.Fatal("turn 1 should have offered OTP")
		}
		if ran {
			t.Fatal("tool must not run before verification")
		}

		// Verify the code.
		sendFrame(t, transport, map[string]any{"action": "verify_otp", "requestId": "r-otp", "sessionId": sessionID, "code": "123456"})
		if ev := nextEv(t, transport); ev["type"] != "otp_verified" {
			t.Fatalf("want otp_verified, got %v", ev["type"])
		}

		// Turn 2: now verified → the tool runs.
		sendFrame(t, transport, map[string]any{"action": "send_message", "requestId": "r-2", "sessionId": sessionID, "message": "show my orders"})
		order, required, _ := collectUntilEventual(t, transport)
		if !ran {
			t.Errorf("verified session must run the end_user tool; order %v", order)
		}
		if required != nil {
			t.Errorf("a verified session must NOT be re-offered OTP; order %v", order)
		}
	})
}

// typeIndex returns the position of the event type s in the ordered list xs, or -1.
func typeIndex(xs []string, s string) int {
	for i, x := range xs {
		if x == s {
			return i
		}
	}
	return -1
}
