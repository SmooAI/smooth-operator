package server

import (
	"context"
	"encoding/json"
	"net"
	"net/http"
	"sync"
	"testing"
	"time"

	"github.com/coder/websocket"

	core "github.com/SmooAI/smooth-operator-core/go/core"
)

// burstClient streams one token, signals that the turn is provably in-flight, then —
// once released — blasts far more tokens than the connection's outbound sink can buffer.
// That is the shape that wedges the server when the socket dies mid-stream: the writer
// stops draining, the bounded sink fills, and the turn blocks on a send it can never
// complete.
type burstClient struct {
	started chan struct{}
	release chan struct{}
	tokens  int
	once    sync.Once
}

func newBurstClient(tokens int) *burstClient {
	return &burstClient{started: make(chan struct{}, 1), release: make(chan struct{}), tokens: tokens}
}

func (b *burstClient) Chat(_ context.Context, _ core.ChatRequest) (core.ChatResponse, error) {
	return core.TextResponse("burst reply"), nil
}

func (b *burstClient) ChatStream(_ context.Context, _ core.ChatRequest) (<-chan core.ChatChunk, error) {
	ch := make(chan core.ChatChunk)
	go func() {
		defer close(ch)
		ch <- core.ChatChunk{ContentDelta: "warming up "}
		b.once.Do(func() { b.started <- struct{}{} })
		<-b.release
		for i := 0; i < b.tokens; i++ {
			ch <- core.ChatChunk{ContentDelta: "tok "}
		}
		u := core.Usage{PromptTokens: 1, CompletionTokens: 2}
		ch <- core.ChatChunk{Usage: &u}
	}()
	return ch, nil
}

// dialCapturingTCP dials ls over WebSocket and hands back both the WS conn and the raw
// TCP conn underneath it, so the test can kill the socket with an RST (SetLinger(0))
// rather than a polite close — that is what makes the server's writes fail.
func dialCapturingTCP(t *testing.T, ls *LocalServer) (*websocket.Conn, *net.TCPConn) {
	t.Helper()
	var tcp *net.TCPConn
	httpClient := &http.Client{Transport: &http.Transport{
		DialContext: func(ctx context.Context, network, addr string) (net.Conn, error) {
			c, err := (&net.Dialer{}).DialContext(ctx, network, addr)
			if err != nil {
				return nil, err
			}
			tcp, _ = c.(*net.TCPConn)
			return c, nil
		},
	}}
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	conn, _, err := websocket.Dial(ctx, ls.WSURL(), &websocket.DialOptions{HTTPClient: httpClient})
	if err != nil {
		t.Fatalf("dial: %v", err)
	}
	conn.SetReadLimit(1 << 20)
	if tcp == nil {
		t.Fatal("failed to capture the underlying TCP connection")
	}
	return conn, tcp
}

func writeFrame(t *testing.T, conn *websocket.Conn, frame map[string]any) {
	t.Helper()
	data, err := json.Marshal(frame)
	if err != nil {
		t.Fatalf("marshal frame: %v", err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	if err := conn.Write(ctx, websocket.MessageText, data); err != nil {
		t.Fatalf("write frame: %v", err)
	}
}

// readEventUntil reads events until match returns true, or fails the test.
func readEventUntil(t *testing.T, conn *websocket.Conn, match func(map[string]any) bool) map[string]any {
	t.Helper()
	deadline := time.Now().Add(10 * time.Second)
	for time.Now().Before(deadline) {
		ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		_, data, err := conn.Read(ctx)
		cancel()
		if err != nil {
			t.Fatalf("read event: %v", err)
		}
		var ev map[string]any
		if err := json.Unmarshal(data, &ev); err != nil {
			continue
		}
		if match(ev) {
			return ev
		}
	}
	t.Fatal("timed out waiting for the expected event")
	return nil
}

// TestClientDisconnectMidStreamDoesNotWedgeTurn is the regression for the connection
// deadlock: a client that vanishes mid-turn used to leave the turn goroutine blocked
// forever on a full outbound sink (the writer returned on its first failed write, so
// nothing drained it), which hung WaitForTurns → teardown → the connection's s.conns
// entry → Shutdown, permanently.
//
// Note `go test -race` cannot catch this: a wedged goroutine is not a data race. The
// assertion has to be that Shutdown actually returns.
func TestClientDisconnectMidStreamDoesNotWedgeTurn(t *testing.T) {
	burst := newBurstClient(20000)
	ls, err := SpawnLocal(WithLocalAddr("127.0.0.1:0"), WithLocalChatClient(burst))
	if err != nil {
		t.Fatalf("spawn: %v", err)
	}
	defer ls.Shutdown()

	conn, tcp := dialCapturingTCP(t, ls)

	writeFrame(t, conn, map[string]any{
		"action":    "create_conversation_session",
		"requestId": "req-session",
		"agentId":   "agent-1",
	})
	var sessionID string
	readEventUntil(t, conn, func(ev map[string]any) bool {
		if ev["type"] != "immediate_response" {
			return false
		}
		data, _ := ev["data"].(map[string]any)
		id, _ := data["sessionId"].(string)
		if id == "" {
			return false
		}
		sessionID = id
		return true
	})

	writeFrame(t, conn, map[string]any{
		"action":    "send_message",
		"requestId": "req-turn",
		"sessionId": sessionID,
		"message":   "stream a lot please",
	})

	select {
	case <-burst.started:
	case <-time.After(5 * time.Second):
		t.Fatal("turn never started")
	}

	// Let the burst run so the writer is actively streaming, and read a few frames to
	// prove it is flowing, THEN kill the socket underneath it. SetLinger(0) sends an RST
	// so the server's next conn.Write fails rather than blocking.
	close(burst.release)
	seen := 0
	readEventUntil(t, conn, func(ev map[string]any) bool {
		if ev["type"] == "stream_token" {
			seen++
		}
		return seen >= 20
	})
	if err := tcp.SetLinger(0); err != nil {
		t.Fatalf("set linger: %v", err)
	}
	_ = tcp.Close()

	// With ~20k tokens still queued behind a dead socket, the sink fills immediately.
	// The turn must still terminate, the connection loop must exit, and Shutdown must
	// return. Before the fix this blocks forever.
	done := make(chan error, 1)
	go func() { done <- ls.Shutdown() }()
	select {
	case err := <-done:
		if err != nil {
			t.Fatalf("shutdown errored: %v", err)
		}
	case <-time.After(20 * time.Second):
		t.Fatal("shutdown never returned: the turn goroutine is wedged on a full sink with no reader")
	}
}

// panickyStore panics on the one store call the turn goroutine makes on its own stack
// (persisting the inbound user message) — the stand-in for a host store/tool that blows
// up mid-turn.
type panickyStore struct{ SessionStore }

func (panickyStore) AppendMessage(context.Context, string, MessageDirection, string) (StoredMessage, error) {
	panic("host store exploded")
}

// TestPanickingTurnDoesNotKillTheProcess pins the panic-containment contract: Go's
// default is to take the whole PROCESS down on an unrecovered panic in ANY goroutine,
// and turns run on a bare goroutine — so before the fix one bad host store/tool killed
// the server and dropped every other live connection. The turn must instead settle as a
// clean INTERNAL_ERROR with the connection still usable (a ping still round-trips).
//
// NOTE: this covers panics on the TURN goroutine's own stack. A panic inside a TOOL is
// still fatal, because the engine runs the tool loop on its own recover-less goroutine
// in smooth-operator-core (core/agent.go RunStream) — that fix belongs in the core
// module, not here.
func TestPanickingTurnDoesNotKillTheProcess(t *testing.T) {
	ls, err := SpawnLocal(
		WithLocalAddr("127.0.0.1:0"),
		WithLocalChatClient(newBurstClient(1)),
		WithLocalServerOption(WithSessionStore(panickyStore{NewInMemorySessionStore()})),
	)
	if err != nil {
		t.Fatalf("spawn: %v", err)
	}
	defer ls.Shutdown()

	conn, _ := dialCapturingTCP(t, ls)
	defer conn.Close(websocket.StatusNormalClosure, "")

	writeFrame(t, conn, map[string]any{
		"action":    "create_conversation_session",
		"requestId": "req-session",
		"agentId":   "agent-1",
	})
	var sessionID string
	readEventUntil(t, conn, func(ev map[string]any) bool {
		data, _ := ev["data"].(map[string]any)
		id, _ := data["sessionId"].(string)
		if ev["type"] != "immediate_response" || id == "" {
			return false
		}
		sessionID = id
		return true
	})

	writeFrame(t, conn, map[string]any{
		"action":    "send_message",
		"requestId": "req-turn",
		"sessionId": sessionID,
		"message":   "boom",
	})
	ev := readEventUntil(t, conn, func(ev map[string]any) bool { return ev["type"] == "error" })
	descriptor, _ := ev["error"].(map[string]any)
	if code, _ := descriptor["code"].(string); code != "INTERNAL_ERROR" {
		t.Fatalf("want INTERNAL_ERROR after a panicking turn, got %+v", ev)
	}

	// The connection must still serve frames — the panic took down neither it nor the
	// process.
	writeFrame(t, conn, map[string]any{"action": "ping", "requestId": "req-ping"})
	readEventUntil(t, conn, func(ev map[string]any) bool { return ev["type"] == "pong" })
}
