# smooth-operator server (Go)

<p>
  <a href="https://smoo.ai"><img src="https://img.shields.io/badge/Smoo_AI-platform-00A6A6?style=for-the-badge&labelColor=020618" alt="Smoo AI"></a>
  <a href="https://github.com/SmooAI/smooth-operator/blob/main/LICENSE"><img src="https://img.shields.io/badge/license-MIT-FF6B6C?style=for-the-badge&labelColor=020618" alt="license"></a>
  <a href="https://go.dev"><img src="https://img.shields.io/badge/Go-1.26-00A6A6?style=for-the-badge&labelColor=020618" alt="Go 1.26"></a>
</p>

**Wiring a chat loop is a weekend project. A production agent _server_ is not.**

Sessions that survive a reconnect. A wire protocol your clients can actually speak. Streaming turns you can watch token by token. Tools the model can call — and hard limits on the ones it must never call. Human-in-the-loop when a tool wants to write.

This is that server, in Go. `github.com/SmooAI/smooth-operator/go/server` speaks the [smooth-operator](https://github.com/SmooAI/smooth-operator) wire protocol ([`spec/`](https://github.com/SmooAI/smooth-operator/tree/main/spec)) and wraps the Go agent engine ([`smooth-operator-core/go/core`](https://github.com/SmooAI/smooth-operator-core)) — one `SmoothAgent` per turn. It's the Go sibling of the [Rust](../../rust/smooth-operator-server), [TypeScript](../../typescript/server), [Python](../../python/server), and [C#](../../dotnet/server) servers, all speaking the one protocol.

> Until now Go shipped only a **client** ([`../protocol`](../protocol)). This fills the server gap.

---

## Spin up a real agent server

```go
import "github.com/SmooAI/smooth-operator/go/server"

// Foreground, in-memory, auth off — drains on SIGTERM/SIGINT:
_ = server.ServeLocal(ctx, "127.0.0.1:8787", server.WithLocalChatClient(gatewayClient))

// Or embedded, with a handle:
ls, _ := server.SpawnLocal(server.WithLocalAddr("127.0.0.1:0"), server.WithLocalChatClient(c))
defer ls.Shutdown()
fmt.Println(ls.WSURL()) // ws://127.0.0.1:<port>/ws
```

That's a full agent backend — sessions, streaming turns, tool-calling, citations — on one WebSocket. With no chat client configured, `send_message` settles as a clean protocol `error` rather than panicking or dropping the socket.

Every event the server emits is validated against the **same `spec/` schemas + conformance fixtures** the Rust reference server is held to (via the Go client's `protocol.Validator`), and round-trips back through `protocol.ParseServerEvent`. Conformance is enforced, not assumed.

---

## Extensible — and safe by construction

An agent is only useful when it can *do* things, and only trustworthy when you can say what it may never do. This server gives you both seams.

**Give it your tools.** Any `core.Tool` you register merges with the built-ins for every turn:

```go
type OpenTicket struct{}

func (OpenTicket) Name() string                 { return "open_ticket" }
func (OpenTicket) Description() string           { return "Open a support ticket for the current customer." }
func (OpenTicket) Parameters() map[string]any    { return map[string]any{"type": "object"} }
func (OpenTicket) Execute(ctx context.Context, args map[string]any) (string, error) {
    return fmt.Sprintf("ticket opened: %v", args), nil
}

ls, _ := server.SpawnLocal(
    server.WithLocalChatClient(c),
    server.WithLocalServerOption(server.WithTools([]core.Tool{OpenTicket{}})),
)
```

**Or let it gain tools with no redeploy.** The server hosts [SEP extensions](https://github.com/SmooAI/smooth-operator/blob/main/docs/TOOLS.md) — out-of-process tool providers discovered at runtime, their `ui/confirm` prompts bridged into the protocol's confirmation frames for HITL. Gated: an extension contributes tools **only** if you name it in `SMOOTH_EXTENSIONS_ALLOW`. Nothing loads by default.

**Now declare the lines it can't cross.** Install an `AgentConfigResolver` (`WithAgentConfigResolver`), and every tool — built-in, yours, or from an extension — flows through the same gates:

- **Per-agent allow-list** — an agent's `tool_config.enabledTools` restricts its turn to exactly those tools. Off the list, off the table.
- **The authLevel gate** — a tool tagged `admin` or (unverified) `end_user` is *blocked at call time* on a public agent, via the session's OTP bit or your `SessionAuthenticator` seam. **Fail-closed** — no authenticator means "not authenticated".
- **End-user OTP flow** — a refused `end_user` tool can offer a one-time-code identity flow via the `OtpService` seam (`WithOtpService`); the server never generates, delivers, or holds a code.

You decide what the agent can touch; the runner enforces it.

---

## What's shipped

- `SessionStore` / `InMemorySessionStore` — sessions + conversation message logs.
- `FrameDispatcher` — validates inbound frames and routes them (`create_conversation_session` → store, `send_message` → `TurnRunner`, `get_session` → store, `cancel` → abort the in-flight turn, `ping` → pong).
- **Turn cancellation** (the "Stop button") — one active turn per connection, tracked with its own cancellable context. `cancel` aborts it and emits the terminal `cancelled` event (`status: 499`) in place of the `eventual_response`; the partial assistant reply is discarded (the user message stays persisted). A second `send_message` mid-turn is rejected with `TURN_IN_PROGRESS`; a client disconnect aborts the turn too — unlike the SIGTERM drain, which lets it finish.
- `TurnRunner` — drives one turn: replay history into a `core.SmoothAgent` thread, consume `RunStream`, emit a `stream_token` per text delta and a `stream_chunk` per tool call / result, persist the reply, return the terminal `eventual_response` (with citations).
- `AuthVerifier` seam — a default permissive verifier and a `LocalTokenVerifier` (HS256 JWT, fail-closed), chosen at connect from the `?token=` slot.
- `AgentConfigResolver` seam — resolves a session's `agentId` into its per-agent config (instructions, conversation `Workflow`, greeting, personality, tool allow-list), folded into the turn. A configured `Workflow` runs a stepped, judge-advanced guided-agency flow.
- SEP extension hosting, host tool injection (`WithTools`), and the authLevel gate + OTP flow above.
- WebSocket transport (`github.com/coder/websocket`) — one `/ws` endpoint, a per-connection read loop and a single outbound writer goroutine.
- **Graceful SIGTERM/SIGINT drain** — one shared drain context; each connection finishes its in-flight turn, flushes the terminal event, then detaches and exits.
- `ServeLocal` / `SpawnLocal` — the in-memory, loopback, auth-off entrypoint, embeddable in-process.

Stubbed seams (left open for later phases): `Backplane` (in-memory only — the Redis/NATS cross-pod fan-out is the seam), ACL-filtered retrieval + rerank in the turn.

---

## Five languages, one protocol

The *same* server — same wire protocol, same conformance corpus — exists in five languages. Run it where your stack already lives.

| Language | Server package | Registry |
| --- | --- | --- |
| **Go** | `github.com/SmooAI/smooth-operator/go/server` | in-repo (this module) |
| **Rust** | `smooai-smooth-operator-server` | [crates.io](https://crates.io/crates/smooai-smooth-operator-server) |
| **C# / .NET** | `SmooAI.SmoothOperator.Server` | [in-repo](../../dotnet/server) |
| **TypeScript** | `@smooai/smooth-operator-server` | [in-repo](../../typescript/server) |
| **Python** | `smooai-smooth-operator-server` | [in-repo](../../python/server) |

Every native client — [TypeScript](https://www.npmjs.com/package/@smooai/smooth-operator), Go, .NET, Python, Rust — connects to any of them unmodified.

---

## Build / test

```bash
cd go/server
go mod tidy && gofmt -w . && go vet ./... && go test -race ./...
```

This package is its own Go module (`github.com/SmooAI/smooth-operator/go/server`) — it depends on the engine module, while the published client module ([`../`](../)) stays dependency-light.

---

Part of the **[smooth-operator](https://github.com/SmooAI/smooth-operator)** service — Smoo AI's polyglot AI agent service. Don't want to run it yourself? **[lom.smoo.ai](https://lom.smoo.ai)** hosts it for you.

## License

MIT © 2026 Smoo AI. See [LICENSE](https://github.com/SmooAI/smooth-operator/blob/main/LICENSE).
