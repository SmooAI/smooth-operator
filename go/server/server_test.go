package server

import (
	"context"
	"strings"
	"testing"
	"time"

	core "github.com/SmooAI/smooth-operator-core/go/core"
	"github.com/SmooAI/smooth-operator/go/protocol"
)

// newClient dials ls with the default WebSocket transport and connects a protocol
// client, registering cleanup.
func newClient(t *testing.T, ls *LocalServer) *protocol.Client {
	t.Helper()
	transport := protocol.NewWebSocketTransport(ls.WSURL(), nil)
	client, err := protocol.New(protocol.Options{Transport: transport})
	if err != nil {
		t.Fatalf("new client: %v", err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	if err := client.Connect(ctx); err != nil {
		t.Fatalf("connect: %v", err)
	}
	t.Cleanup(func() { _ = client.Close() })
	return client
}

// TestServeLocalBootsAndAcceptsConnection asserts a local-flavor server starts on an
// ephemeral port and accepts a WebSocket connection (a ping round-trips).
func TestServeLocalBootsAndAcceptsConnection(t *testing.T) {
	ls, err := SpawnLocal(WithLocalAddr("127.0.0.1:0"))
	if err != nil {
		t.Fatalf("spawn: %v", err)
	}
	defer ls.Shutdown()

	if !strings.HasPrefix(ls.WSURL(), "ws://127.0.0.1:") {
		t.Fatalf("unexpected ws url: %q", ls.WSURL())
	}

	client := newClient(t, ls)
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	if _, err := client.Ping(ctx); err != nil {
		t.Fatalf("ping: %v", err)
	}
}

// TestTurnRoundTrip drives create_conversation_session → send_message against the
// engine on a MockLlmProvider and asserts the streamed tokens concatenate to the
// scripted reply and the terminal eventual_response carries it.
func TestTurnRoundTrip(t *testing.T) {
	const reply = "hello there friend, how are you"
	mock := core.NewMockLlmProvider().PushResponse(core.WithUsage(core.TextResponse(reply), 10, 7))

	ls, err := SpawnLocal(WithLocalAddr("127.0.0.1:0"), WithLocalChatClient(mock))
	if err != nil {
		t.Fatalf("spawn: %v", err)
	}
	defer ls.Shutdown()

	client := newClient(t, ls)
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()

	session, err := client.CreateConversationSession(ctx, protocol.CreateConversationSessionParams{
		AgentID: "agent-1", UserName: "Alice", UserEmail: "alice@example.com",
	})
	if err != nil {
		t.Fatalf("create session: %v", err)
	}
	if session.SessionID == "" || session.ConversationID == "" {
		t.Fatalf("session missing ids: %+v", session)
	}

	turn := client.SendMessage(protocol.SendMessageParams{SessionID: session.SessionID, Message: "hi"})

	var streamed strings.Builder
	tokenEvents := 0
	for ev := range turn.Events() {
		if ev.Type == protocol.EventStreamToken {
			tok, derr := ev.AsStreamToken()
			if derr != nil {
				t.Fatalf("decode stream_token: %v", derr)
			}
			if tok.Token != nil {
				streamed.WriteString(*tok.Token)
				tokenEvents++
			}
		}
	}
	if tokenEvents < 2 {
		t.Fatalf("want >=2 stream_token events, got %d", tokenEvents)
	}
	if streamed.String() != reply {
		t.Fatalf("streamed tokens = %q, want %q", streamed.String(), reply)
	}

	final, err := turn.Wait(ctx)
	if err != nil {
		t.Fatalf("turn wait: %v", err)
	}
	if final.Data.Data.MessageID == "" {
		t.Fatalf("terminal response missing messageId: %+v", final)
	}
	parts := responseParts(t, final)
	if len(parts) == 0 || parts[0] != reply {
		t.Fatalf("eventual_response responseParts = %v, want [%q]", parts, reply)
	}
}

// TestSendMessageWithoutEngineErrorsCleanly asserts that, with no chat client
// configured, send_message settles as a clean protocol error (not a dropped socket).
func TestSendMessageWithoutEngineErrorsCleanly(t *testing.T) {
	ls, err := SpawnLocal(WithLocalAddr("127.0.0.1:0")) // no chat client
	if err != nil {
		t.Fatalf("spawn: %v", err)
	}
	defer ls.Shutdown()

	client := newClient(t, ls)
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()

	session, err := client.CreateConversationSession(ctx, protocol.CreateConversationSessionParams{AgentID: "agent-1"})
	if err != nil {
		t.Fatalf("create session: %v", err)
	}
	turn := client.SendMessage(protocol.SendMessageParams{SessionID: session.SessionID, Message: "hi"})
	if _, err := turn.Wait(ctx); err == nil {
		t.Fatal("expected a protocol error with no engine configured, got nil")
	}
}

// TestUnknownSessionErrors asserts send_message against an unknown session yields a
// NOT_FOUND error rather than running a turn.
func TestUnknownSessionErrors(t *testing.T) {
	ls, err := SpawnLocal(WithLocalAddr("127.0.0.1:0"), WithLocalChatClient(core.NewMockLlmProvider().PushText("hi")))
	if err != nil {
		t.Fatalf("spawn: %v", err)
	}
	defer ls.Shutdown()

	client := newClient(t, ls)
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	turn := client.SendMessage(protocol.SendMessageParams{SessionID: "does-not-exist", Message: "hi"})
	_, err = turn.Wait(ctx)
	if err == nil {
		t.Fatal("expected NOT_FOUND error, got nil")
	}
	var pe *protocol.ProtocolError
	if !asProtocolError(err, &pe) || pe.Code != "NOT_FOUND" {
		t.Fatalf("expected NOT_FOUND protocol error, got %v", err)
	}
}

// responseParts pulls the GeneralAgentResponse.responseParts out of the terminal
// event's untyped response payload.
func responseParts(t *testing.T, final protocol.EventualResponse) []string {
	t.Helper()
	resp, ok := final.Data.Data.Response.(map[string]any)
	if !ok {
		t.Fatalf("response not an object: %T", final.Data.Data.Response)
	}
	raw, ok := resp["responseParts"].([]any)
	if !ok {
		t.Fatalf("responseParts missing/wrong type: %v", resp["responseParts"])
	}
	parts := make([]string, 0, len(raw))
	for _, p := range raw {
		if s, ok := p.(string); ok {
			parts = append(parts, s)
		}
	}
	return parts
}

func asProtocolError(err error, target **protocol.ProtocolError) bool {
	pe, ok := err.(*protocol.ProtocolError)
	if ok {
		*target = pe
	}
	return ok
}
