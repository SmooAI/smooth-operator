package server

import (
	"context"
	"encoding/json"
	"net"
	"net/http"
	"sync"
	"time"

	core "github.com/SmooAI/smooth-operator-core/go/core"
	"github.com/coder/websocket"
	"github.com/google/uuid"
)

// Server is the WebSocket smooth-operator server: one /ws endpoint, one goroutine per
// connection. The Go analog of the Rust axum server (server.rs) and the C#
// SmoothOperatorWebSocketExtensions. Per connection we run a single outbound writer
// goroutine fed by a channel and a read loop that dispatches inbound frames — so a
// streaming turn can fire many events while the connection is still reading.
type Server struct {
	store     SessionStore
	client    core.ChatClient
	auth      AuthVerifier
	backplane Backplane
	systemP   string

	// drainCtx is the single shutdown source for the whole server (one source,
	// default uncancelled). Each connection loop selects on its Done() so an
	// in-flight turn can finish before the loop exits (graceful SIGTERM drain).
	drainCtx    context.Context
	drainCancel context.CancelFunc

	// conns tracks live connection goroutines so Shutdown can wait for in-flight turns
	// to drain before the HTTP server stops.
	conns sync.WaitGroup

	mu     sync.Mutex
	closed bool
}

// Option configures a Server.
type Option func(*Server)

// WithSessionStore overrides the session store (default: a fresh in-memory store).
func WithSessionStore(s SessionStore) Option { return func(srv *Server) { srv.store = s } }

// WithChatClient sets the engine chat client used to run turns. With none, send_message
// settles as a clean protocol error (the keyless path).
func WithChatClient(c core.ChatClient) Option { return func(srv *Server) { srv.client = c } }

// WithAuth overrides the connection auth verifier (default: PermissiveVerifier).
func WithAuth(v AuthVerifier) Option { return func(srv *Server) { srv.auth = v } }

// WithBackplane overrides the connection backplane (default: in-memory).
func WithBackplane(b Backplane) Option { return func(srv *Server) { srv.backplane = b } }

// WithSystemPrompt overrides the agent system prompt (default: support-agent prompt).
func WithSystemPrompt(p string) Option { return func(srv *Server) { srv.systemP = p } }

// New builds a Server with the given options, defaulting every collaborator to its
// in-memory / permissive reference impl so New() with no options is a working server.
func New(opts ...Option) *Server {
	drainCtx, drainCancel := context.WithCancel(context.Background())
	srv := &Server{
		store:       NewInMemorySessionStore(),
		auth:        PermissiveVerifier{},
		backplane:   NewInMemoryBackplane(),
		drainCtx:    drainCtx,
		drainCancel: drainCancel,
	}
	for _, opt := range opts {
		opt(srv)
	}
	return srv
}

// Handler returns the http.Handler serving the /ws WebSocket endpoint. Exposed so a
// host can mount it on its own mux / TLS server.
func (s *Server) Handler() http.Handler {
	mux := http.NewServeMux()
	mux.HandleFunc("/ws", s.handleWS)
	return mux
}

// Shutdown stops accepting work and signals every connection loop to drain: in-flight
// turns finish, then the loops exit and detach. Idempotent.
func (s *Server) Shutdown() {
	s.mu.Lock()
	if s.closed {
		s.mu.Unlock()
		return
	}
	s.closed = true
	s.mu.Unlock()
	s.drainCancel()
}

// handleWS upgrades an HTTP request on /ws to a WebSocket. The bearer token (if any)
// rides on ?token= (browsers can't set WS handshake headers) and is resolved to an
// AccessContext at connect, threaded into every turn.
func (s *Server) handleWS(w http.ResponseWriter, r *http.Request) {
	s.mu.Lock()
	closed := s.closed
	s.mu.Unlock()
	if closed {
		http.Error(w, "server draining", http.StatusServiceUnavailable)
		return
	}

	conn, err := websocket.Accept(w, r, nil)
	if err != nil {
		return // Accept already wrote the error response.
	}
	conn.SetReadLimit(1 << 20) // agent stream_chunk state frames can be large.

	// Track the connection so Shutdown can wait for its in-flight turn to drain. The
	// HTTP handler goroutine runs the loop directly (so the response stays open).
	s.conns.Add(1)
	defer s.conns.Done()

	access := s.auth.Resolve(r.URL.Query().Get("token"))
	s.connectionLoop(conn, access)
}

// connectionLoop drives one WebSocket connection: a single writer goroutine fed by an
// outbound sink channel, and a read loop that dispatches inbound frames. The Go analog
// of the Rust connection_loop. Applies the graceful-drain spec: the loop selects on the
// server drain context vs the next inbound frame, with the turn dispatch awaited INSIDE
// the frame branch so an in-flight turn finishes; ctx is checked FIRST each iteration
// (Go select is random on ties, so the cancel is preferred explicitly). A backplane
// detach always runs after the loop exits (the detach-after-loop guarantee).
func (s *Server) connectionLoop(conn *websocket.Conn, access AccessContext) {
	connID := uuid.NewString()
	// ioCtx bounds the socket reads/writes for THIS connection and is cancelled ONLY at
	// teardown — NOT by the server drain. This is the key to graceful drain: when the
	// server starts draining we stop accepting NEW frames, but an in-flight turn's events
	// must still flush over the socket, so the writer can't be tied to the drain signal.
	ioCtx, ioCancel := context.WithCancel(context.Background())
	defer ioCancel()

	// Outbound sink → single writer goroutine. WebSocket writes aren't safe to call
	// concurrently, so every event funnels through one writer (Rust sink_tx + writer
	// split / C# channel + writer task). Buffered so a streaming turn doesn't block on
	// a slow socket for the common case.
	sink := make(chan map[string]any, 64)
	var writerWG sync.WaitGroup
	writerWG.Add(1)
	go func() {
		defer writerWG.Done()
		for event := range sink {
			data, err := json.Marshal(event)
			if err != nil {
				continue
			}
			if err := conn.Write(ioCtx, websocket.MessageText, data); err != nil {
				return
			}
		}
	}()

	// sendMu serializes sends on the sink + guards the closed flag so a backplane
	// publish (from another goroutine) and the read loop can't send on a closed channel.
	var sendMu sync.Mutex
	sinkClosed := false
	send := func(event map[string]any) {
		sendMu.Lock()
		defer sendMu.Unlock()
		if sinkClosed {
			return
		}
		select {
		case sink <- event:
		case <-ioCtx.Done():
		}
	}

	// Register this connection's outbound sink with the backplane so events published
	// from anywhere can reach it. Detached after the loop exits (defer), always — the
	// detach-after-loop guarantee.
	s.backplane.Attach(s.drainCtx, connID, send)
	defer s.backplane.Detach(context.Background(), connID)

	dispatcher := NewFrameDispatcher(s.store, s.client, access, s.systemP)

	// teardown closes the writer sink once (under sendMu, so an in-flight send can't race
	// the close), waits for the writer to drain, and closes the socket.
	teardown := func(status websocket.StatusCode, reason string) {
		sendMu.Lock()
		if !sinkClosed {
			sinkClosed = true
			close(sink)
		}
		sendMu.Unlock()
		writerWG.Wait()
		_ = conn.Close(status, reason)
	}

	// readResult carries one inbound frame (or the read error) from the reader goroutine.
	type readResult struct {
		typ  websocket.MessageType
		data []byte
		err  error
	}

	for {
		// Read one frame in a goroutine so the blocking read is selectable against the
		// drain signal. The read uses ioCtx (cancelled only at teardown), so a drain
		// doesn't abort a read that's already returning a frame.
		next := make(chan readResult, 1)
		go func() {
			typ, data, err := conn.Read(ioCtx)
			next <- readResult{typ, data, err}
		}()

		// Check drain FIRST: Go's select is random on a tie, so when the server is
		// draining we prefer to stop rather than process another frame. No turn is
		// in-flight at this point (dispatch below is awaited inline before we loop back),
		// so stopping here finishes the drain cleanly.
		select {
		case <-s.drainCtx.Done():
			// Server draining → stop accepting frames. Any turn dispatched on a prior
			// iteration already completed (Dispatch is awaited), and its events have been
			// queued to the still-open writer; teardown flushes them before closing.
			teardown(websocket.StatusGoingAway, "server draining")
			return
		case r := <-next:
			if r.err != nil {
				// Read error / client close / teardown cancel → tear down.
				teardown(websocket.StatusNormalClosure, "bye")
				return
			}
			if r.typ != websocket.MessageText {
				send(errorEvent("", "VALIDATION_ERROR", "binary frames are not supported; send JSON text frames"))
				continue
			}
			// Dispatch the turn INLINE (awaited) with ioCtx — NOT the drain ctx — so a
			// drain that fires mid-turn doesn't abort it: a streaming send_message turn
			// fires many events through send and only returns once the terminal
			// eventual_response is emitted, so an in-flight turn always finishes.
			dispatcher.Dispatch(ioCtx, r.data, send)
		}
	}
}

// listenAndServe binds addr and serves until ctx (the drain ctx) is cancelled. Shared
// by ServeLocal; returns the resolved bound address via the addrFn callback before
// blocking, so callers (and tests) can read an ephemeral port.
func (s *Server) listenAndServe(addr string, addrFn func(net.Addr)) error {
	ln, err := net.Listen("tcp", addr)
	if err != nil {
		return err
	}
	if addrFn != nil {
		addrFn(ln.Addr())
	}
	httpServer := &http.Server{Handler: s.Handler()}
	// On drain: stop the listener (no new connections), let live connection loops drain
	// their in-flight turns and exit on the drain signal, then close the HTTP server.
	// Closing the listener via Shutdown (not Close) leaves active connections alone so
	// their loops flush the terminal turn event before self-closing.
	go func() {
		<-s.drainCtx.Done()
		// Stop accepting; the loops self-terminate on drainCtx and the conns WaitGroup
		// gates until their in-flight turns have flushed.
		shutCtx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
		defer cancel()
		s.conns.Wait()
		_ = httpServer.Shutdown(shutCtx)
	}()
	err = httpServer.Serve(ln)
	if err == http.ErrServerClosed {
		return nil
	}
	return err
}
