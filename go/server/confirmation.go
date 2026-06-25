package server

import "sync"

// Write-confirmation HITL — the per-connection pending-confirmation registry.
//
// When an agent turn calls a tool that requires human approval, the turn PARKS
// inside the engine's HumanGate and the runner registers a resolver here, keyed
// by sessionId. A subsequent confirm_tool_action frame on the same connection
// looks the session up, resolves the channel with the verdict, and the parked
// turn resumes (runs the tool on approve; skips it with a rejection result on
// deny).
//
// The Go analog of the Rust AppState pending-confirmation map
// (register_confirmation / take_confirmation / clear_confirmation) and the
// Python ConfirmationRegistry. Keyed by session so each session has at most one
// outstanding confirmation; an empty registry means no turn is parked (the
// default — behavior identical to before HITL).
//
// Unlike the Python registry (single-threaded under the asyncio loop), the Go
// registry is touched from two goroutines — the parked turn (which runs in its
// own goroutine so the read loop stays free) registers + awaits, while the read
// loop's confirm_tool_action handler resolves — so every method is mutex-guarded.
type ConfirmationRegistry struct {
	mu sync.Mutex
	// pending maps sessionId → the buffered verdict channel a parked turn awaits.
	// true = approved, false = rejected. At most one per session. Buffered (cap 1)
	// so resolve never blocks even if the parked turn has already given up.
	pending map[string]chan bool
}

// NewConfirmationRegistry builds an empty registry (no turn parked).
func NewConfirmationRegistry() *ConfirmationRegistry {
	return &ConfirmationRegistry{pending: map[string]chan bool{}}
}

// Register registers (and returns) a fresh verdict channel for sessionID. Any
// prior pending channel for the session is rejected (resolved false) first, so a
// stale parked turn can never be left dangling and the newest confirmation always
// wins — mirrors the Rust register_confirmation taking over a prior sender.
func (r *ConfirmationRegistry) Register(sessionID string) chan bool {
	r.mu.Lock()
	defer r.mu.Unlock()
	if prior, ok := r.pending[sessionID]; ok {
		// Reject the stale waiter (non-blocking: the channel is buffered cap 1).
		select {
		case prior <- false:
		default:
		}
		delete(r.pending, sessionID)
	}
	ch := make(chan bool, 1)
	r.pending[sessionID] = ch
	return ch
}

// Resolve resolves the parked turn for sessionID with the verdict. Returns true if
// a pending confirmation was resolved, false if none was awaiting (a
// duplicate/stale confirm_tool_action → NO_PENDING_CONFIRMATION). Taking the
// channel out makes a duplicate confirm a clean no-op (mirrors the Rust
// take_confirmation).
func (r *ConfirmationRegistry) Resolve(sessionID string, approved bool) bool {
	r.mu.Lock()
	defer r.mu.Unlock()
	ch, ok := r.pending[sessionID]
	if !ok {
		return false
	}
	delete(r.pending, sessionID)
	// Buffered cap 1 → this send never blocks; the parked turn receives the verdict.
	ch <- approved
	return true
}

// Clear drops any registered channel for sessionID (turn ended), so a stale entry
// can't mis-route a later confirmation. Idempotent.
func (r *ConfirmationRegistry) Clear(sessionID string) {
	r.mu.Lock()
	defer r.mu.Unlock()
	delete(r.pending, sessionID)
}

// RejectAll resolves every outstanding confirmation as REJECTED (deny). Called
// when a connection is torn down (close / graceful drain) so any turn parked on a
// confirmation unparks and finishes cleanly — fail closed (a write is never
// auto-approved on disconnect) and never leave a turn hung forever.
func (r *ConfirmationRegistry) RejectAll() {
	r.mu.Lock()
	defer r.mu.Unlock()
	for sid, ch := range r.pending {
		select {
		case ch <- false:
		default:
		}
		delete(r.pending, sid)
	}
}
