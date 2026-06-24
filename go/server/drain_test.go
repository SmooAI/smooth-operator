package server

import (
	"context"
	"sync"
	"testing"
	"time"

	core "github.com/SmooAI/smooth-operator-core/go/core"
	"github.com/SmooAI/smooth-operator/go/protocol"
)

// gatedClient is a StreamingChatClient that signals when a turn's stream opens, then
// blocks emitting its terminal usage chunk until released — so a test can call
// Shutdown() while a turn is provably in-flight and assert the turn still finishes.
type gatedClient struct {
	started chan struct{} // closed-ish: a value is sent once when ChatStream opens
	release chan struct{} // the test closes this to let the stream complete
	once    sync.Once
}

func newGatedClient() *gatedClient {
	return &gatedClient{started: make(chan struct{}, 1), release: make(chan struct{})}
}

func (g *gatedClient) Chat(_ context.Context, _ core.ChatRequest) (core.ChatResponse, error) {
	return core.TextResponse("drained reply"), nil
}

func (g *gatedClient) ChatStream(_ context.Context, _ core.ChatRequest) (<-chan core.ChatChunk, error) {
	ch := make(chan core.ChatChunk)
	go func() {
		defer close(ch)
		ch <- core.ChatChunk{ContentDelta: "drained reply"}
		g.once.Do(func() { g.started <- struct{}{} }) // turn is now provably in-flight
		<-g.release                                   // block here until the test releases
		u := core.Usage{PromptTokens: 1, CompletionTokens: 2}
		ch <- core.ChatChunk{Usage: &u}
	}()
	return ch, nil
}

// observableBackplane wraps a Backplane and records the connection ids that were
// detached, so the drain test can assert the detach-after-loop guarantee ran.
type observableBackplane struct {
	Backplane
	mu        sync.Mutex
	detached  map[string]struct{}
	detachedC chan string
}

func newObservableBackplane() *observableBackplane {
	return &observableBackplane{
		Backplane: NewInMemoryBackplane(),
		detached:  map[string]struct{}{},
		detachedC: make(chan string, 4),
	}
}

func (o *observableBackplane) Detach(ctx context.Context, connID string) {
	o.Backplane.Detach(ctx, connID)
	o.mu.Lock()
	o.detached[connID] = struct{}{}
	o.mu.Unlock()
	select {
	case o.detachedC <- connID:
	default:
	}
}

func (o *observableBackplane) detachCount() int {
	o.mu.Lock()
	defer o.mu.Unlock()
	return len(o.detached)
}

// TestGracefulDrainFinishesInFlightTurnThenDetaches asserts the SIGTERM-drain spec:
// with a turn provably in-flight, calling Shutdown() lets that turn finish (the
// terminal eventual_response still arrives), the connection loop then exits, and the
// backplane detach runs. Run under -race, this also guards the sink/writer teardown.
func TestGracefulDrainFinishesInFlightTurnThenDetaches(t *testing.T) {
	gated := newGatedClient()
	backplane := newObservableBackplane()

	ls, err := SpawnLocal(
		WithLocalAddr("127.0.0.1:0"),
		WithLocalChatClient(gated),
		WithLocalServerOption(WithBackplane(backplane)),
	)
	if err != nil {
		t.Fatalf("spawn: %v", err)
	}
	// Shutdown is driven explicitly below; the deferred call is the idempotent backstop.
	defer ls.Shutdown()

	client := newClient(t, ls)
	ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
	defer cancel()

	session, err := client.CreateConversationSession(ctx, protocol.CreateConversationSessionParams{AgentID: "agent-1"})
	if err != nil {
		t.Fatalf("create session: %v", err)
	}

	turn := client.SendMessage(protocol.SendMessageParams{SessionID: session.SessionID, Message: "hi"})

	// Wait until the turn is provably in-flight (the engine stream has opened and is
	// blocked on release), THEN trigger the drain.
	select {
	case <-gated.started:
	case <-time.After(5 * time.Second):
		t.Fatal("turn never started")
	}

	drained := make(chan error, 1)
	go func() { drained <- ls.Shutdown() }()

	// The in-flight turn must still complete after the drain begins: release it and
	// assert the terminal response arrives.
	close(gated.release)

	final, err := turn.Wait(ctx)
	if err != nil {
		t.Fatalf("in-flight turn did not finish after drain: %v", err)
	}
	if final.Data.Data.MessageID == "" {
		t.Fatalf("drained turn missing terminal messageId: %+v", final)
	}

	// The connection loop exits and the detach-after-loop runs.
	select {
	case <-backplane.detachedC:
	case <-time.After(5 * time.Second):
		t.Fatal("connection was never detached after drain")
	}
	if backplane.detachCount() == 0 {
		t.Fatal("expected at least one detach")
	}

	select {
	case err := <-drained:
		if err != nil {
			t.Fatalf("server shutdown errored: %v", err)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("server shutdown did not return")
	}
}
