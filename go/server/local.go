package server

import (
	"context"
	"fmt"
	"net"
	"os"
	"os/signal"
	"sync"
	"syscall"

	core "github.com/SmooAI/smooth-operator-core/go/core"
)

// DefaultLocalAddr is the address the local flavor binds when the caller gives none —
// loopback on the canonical WebSocket port. Mirrors the Rust DEFAULT_LOCAL_ADDR.
const DefaultLocalAddr = "127.0.0.1:8787"

// LocalServer is a running local-flavor server handle: its bound address + a graceful
// shutdown switch. The Go analog of the Rust LocalServer. In-memory everything, auth
// off by default — embeddable in-process, no external services.
type LocalServer struct {
	srv  *Server
	addr net.Addr

	// served is closed once the serve loop exits; serveErr then holds its result. This
	// lets Shutdown be called any number of times and from multiple goroutines — every
	// caller waits on the same channel and reads the same cached result (a single
	// chan-error can only deliver its value once).
	served   chan struct{}
	serveErr error
	shutOnce sync.Once
}

// LocalOption configures a local-flavor server before it spawns.
type LocalOption func(*localConfig)

type localConfig struct {
	addr    string
	options []Option
}

// WithLocalAddr binds the local server on addr instead of DefaultLocalAddr. Use
// "127.0.0.1:0" for an ephemeral port (read it back from LocalServer.Addr).
func WithLocalAddr(addr string) LocalOption {
	return func(c *localConfig) { c.addr = addr }
}

// WithLocalChatClient sets the engine chat client for the local server (e.g. a
// MockLlmProvider in tests, or a gateway client). With none, send_message errors cleanly.
func WithLocalChatClient(client core.ChatClient) LocalOption {
	return func(c *localConfig) { c.options = append(c.options, WithChatClient(client)) }
}

// WithLocalServerOption threads an arbitrary Server Option (store, auth, backplane,
// system prompt) through to the local server.
func WithLocalServerOption(opt Option) LocalOption {
	return func(c *localConfig) { c.options = append(c.options, opt) }
}

// SpawnLocal binds and serves a local-flavor server in the background, returning a
// handle carrying the REAL bound address (resolved even for port 0) and a graceful
// shutdown switch. In-memory everything, auth off by default. Mirrors the Rust
// LocalServerBuilder::spawn.
func SpawnLocal(opts ...LocalOption) (*LocalServer, error) {
	cfg := localConfig{addr: DefaultLocalAddr}
	for _, opt := range opts {
		opt(&cfg)
	}

	srv := New(cfg.options...)
	ls := &LocalServer{srv: srv, served: make(chan struct{})}

	// Bind synchronously so the handle reports the real address (and a bind failure
	// surfaces before we report success), then serve in the background.
	bound := make(chan net.Addr, 1)
	go func() {
		err := srv.listenAndServe(cfg.addr, func(a net.Addr) { bound <- a })
		ls.serveErr = err
		close(ls.served)
	}()
	select {
	case a := <-bound:
		ls.addr = a
	case <-ls.served:
		// listenAndServe returned before binding → the bind failed.
		err := ls.serveErr
		if err == nil {
			err = fmt.Errorf("server: local server exited before binding")
		}
		return nil, err
	}
	return ls, nil
}

// Addr is the real address the server bound on (the concrete ephemeral port when port 0
// was requested).
func (l *LocalServer) Addr() net.Addr { return l.addr }

// WSURL is the ws://<addr>/ws URL clients connect to.
func (l *LocalServer) WSURL() string { return fmt.Sprintf("ws://%s/ws", l.addr.String()) }

// Shutdown signals graceful shutdown (drain in-flight turns) and awaits the serve
// loop's clean exit. Idempotent and safe to call concurrently: every caller waits on
// the same served channel and gets the same cached result.
func (l *LocalServer) Shutdown() error {
	l.shutOnce.Do(func() { l.srv.Shutdown() })
	<-l.served
	return l.serveErr
}

// ServeLocal runs a local-flavor server to completion (blocks) on addr, draining
// gracefully on SIGTERM/SIGINT. The one-command foreground entry point — the Go analog
// of the Rust serve_local. In-memory everything, auth off by default.
func ServeLocal(ctx context.Context, addr string, opts ...LocalOption) error {
	if addr == "" {
		addr = DefaultLocalAddr
	}
	all := append([]LocalOption{WithLocalAddr(addr)}, opts...)
	ls, err := SpawnLocal(all...)
	if err != nil {
		return err
	}
	fmt.Printf("smooth-operator-server (local flavor) listening on %s\n", ls.WSURL())

	// Drain on SIGTERM/SIGINT or when ctx is cancelled (one shutdown source applied,
	// matching the server's single drain context).
	sig := make(chan os.Signal, 1)
	signal.Notify(sig, syscall.SIGTERM, syscall.SIGINT)
	defer signal.Stop(sig)

	select {
	case <-ctx.Done():
	case <-sig:
	}
	return ls.Shutdown()
}
