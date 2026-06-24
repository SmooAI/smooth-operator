# smooth-operator server (Go)

The **smooth-operator service in Go** — the native analog of the Rust
[`smooth-operator-server`](../../rust/smooth-operator-server) and the C#
[`SmooAI.SmoothOperator.Server`](../../dotnet/server). It wraps the Go agent engine
([`github.com/SmooAI/smooth-operator-core/go/core`](https://github.com/SmooAI/smooth-operator-core))
and adds the *system* around it: conversation sessions, the schema-driven WebSocket
protocol, and streaming turns. Until now Go shipped only a **client**
([`../protocol`](../protocol)); this fills the server gap.

Conformance is enforced: every event the server emits is validated against the **same
`spec/` schemas + conformance fixtures** the Rust reference server is held to (via the
Go client's `protocol.Validator`), and round-trips back through `protocol.ParseServerEvent`.

## Status — MVP (the protocol runner)

Shipped:

- `SessionStore` / `InMemorySessionStore` — sessions + conversation message logs.
- `FrameDispatcher` — validates inbound frames and routes them: `create_conversation_session`
  → store, `send_message` (+ `requestId`) → `TurnRunner`, `get_session` → store, `ping` → pong.
- `TurnRunner` — drives one `send_message` turn: replay prior history into a
  `core.SmoothAgent` thread, consume `RunStream`, emit a `stream_token` per text delta and
  a `stream_chunk` per tool call / tool result, persist the reply, return the terminal
  `eventual_response` (with a `citations` seam).
- `AuthVerifier` seam — a default permissive verifier (anonymous / org-public) and a
  `LocalTokenVerifier` (HS256 JWT, fail-closed), chosen at connect from the `?token=` slot.
- WebSocket transport (`github.com/coder/websocket`) — one `/ws` endpoint, a per-connection
  read loop and a single outbound writer goroutine fed by a channel.
- **Graceful SIGTERM/SIGINT drain** — one shared drain context; each connection finishes its
  in-flight turn, flushes the terminal event, then detaches from the backplane and exits.
- `ServeLocal` / `SpawnLocal` — an in-memory, loopback, auth-off entrypoint (mirrors the Rust
  `local.rs`), embeddable in-process.

Stubbed seams (left open for later phases): `Backplane` (in-memory only — the Redis/NATS
cross-pod fan-out is the seam), ACL-filtered retrieval + rerank in the turn, and HITL
tool-confirmation.

## Run it

```go
// Foreground, drains on SIGTERM/SIGINT:
_ = server.ServeLocal(ctx, "127.0.0.1:8787", server.WithLocalChatClient(gatewayClient))

// Or embedded, with a handle:
ls, _ := server.SpawnLocal(server.WithLocalAddr("127.0.0.1:0"), server.WithLocalChatClient(c))
defer ls.Shutdown()
fmt.Println(ls.WSURL()) // ws://127.0.0.1:<port>/ws
```

With no chat client configured, `send_message` settles as a clean protocol `error`
(the keyless path) rather than panicking or dropping the socket.

## Build / test

```bash
cd go/server
go mod tidy && gofmt -w . && go vet ./... && go test -race ./...
```

This package is its own Go module (`github.com/SmooAI/smooth-operator/go/server`) — it
depends on the engine module, while the published client module ([`../`](../)) stays
dependency-light. The client module is consumed locally via a `replace` directive.
