# smooai-smooth-operator-server

<p>
  <a href="https://smoo.ai"><img src="https://img.shields.io/badge/Smoo_AI-platform-00A6A6?style=for-the-badge&labelColor=020618" alt="Smoo AI"></a>
  <a href="https://crates.io/crates/smooai-smooth-operator-server"><img src="https://img.shields.io/crates/v/smooai-smooth-operator-server?style=for-the-badge&labelColor=020618&color=00A6A6" alt="crates.io"></a>
  <a href="https://docs.rs/smooai-smooth-operator-server"><img src="https://img.shields.io/badge/docs.rs-smooth--operator--server-F49F0A?style=for-the-badge&labelColor=020618" alt="docs.rs"></a>
  <a href="https://github.com/SmooAI/smooth-operator/blob/main/LICENSE"><img src="https://img.shields.io/badge/license-MIT-FF6B6C?style=for-the-badge&labelColor=020618" alt="license"></a>
</p>

**Wiring a chat loop is a weekend project. A production agent _server_ is not.**

Sessions that survive a reconnect. A wire protocol your clients can actually speak. Streaming turns you can watch token by token. Tools the model can call — and hard limits on the ones it must never call. Retrieval that respects who's asking. Human-in-the-loop when a tool wants to write.

`smooai-smooth-operator-server` is that server, as a Rust crate. It's the **reference WebSocket service** for [smooth-operator](https://github.com/SmooAI/smooth-operator) — the deployment surface that sits in front of the [agent engine](https://crates.io/crates/smooai-smooth-operator-core) and turns it into something you can point a browser, a Lambda, or five languages' worth of clients at.

---

## Spin up a real agent server, in-process

```rust
use smooth_operator_server::local::LocalServer;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // In-memory storage, in-memory backplane, loopback bind, auth off.
    let server = LocalServer::builder().seed_kb(true).spawn().await?;

    println!("smooth-operator on {}", server.ws_url()); // ws://127.0.0.1:8787/ws
    // ... connect a client, drive real streaming turns ...

    server.shutdown().await // graceful drain
}
```

That's a full agent backend — knowledge retrieval, tool-calling, token streaming, session history — on one WebSocket, with no database to provision. Or just run the binary:

```bash
cargo run -p smooai-smooth-operator-server
# → smooth-operator-server (local flavor) listening on ws://127.0.0.1:8787/ws
```

Point it at the [LLM gateway](https://llm.smoo.ai) with `SMOOAI_GATEWAY_KEY` and the turns go live; leave it unset and the whole protocol still works — only `send_message` errors cleanly until a key is present. The local flavor can also serve the embeddable chat widget same-origin (`.serve_widget(...)`) or your own SPA (`.serve_spa(router)`), so one process is the API *and* the UI.

```bash
cargo add smooai-smooth-operator-server
```

---

## One binary, three deployment flavors

The same code runs three ways. The flavor is chosen by config — not a build flag, not a second codebase.

| | **Local** (dev / embed) | **Kubernetes** (self-host) | **AWS serverless** (SST) |
| --- | --- | --- | --- |
| Compute | one in-process server | long-running pods | API GW WebSocket → Lambda |
| Storage | in-memory | Postgres + pgvector | DynamoDB + S3 Vectors |
| Backplane | in-process | Redis / NATS (multi-replica) | API GW connections |
| Run | `cargo run` / `LocalServer` | `helm install` | [`smooth-operator-lambda`](https://crates.io/crates/smooai-smooth-operator-lambda) |

The `StorageAdapter` + backplane + auth seams are what make this one binary instead of three. Set `SMOOTH_AGENT_STORAGE=postgres` and a backplane, and the *same* server graduates from your laptop to a multi-pod cluster. See [`docs/DEPLOY.md`](https://github.com/SmooAI/smooth-operator/blob/main/docs/DEPLOY.md).

---

## Extensible — and safe by construction

An agent is only useful when it can *do* things, and only trustworthy when you can say what it may never do. This server gives you both seams.

**Give it your tools.** Install a `ToolProvider` and the runner asks it, per turn, for the tools to merge with the built-ins — scoped to the turn's org and the caller's entitlements, so a per-org CRM lookup or a ticketing action drops in without the shared crate ever learning your schema.

```rust
use std::sync::Arc;
use async_trait::async_trait;
use smooth_operator::tool_provider::{ToolProvider, ToolProviderContext};
use smooth_operator_core::{Tool, ToolSchema};

struct OpenTicket;

#[async_trait]
impl Tool for OpenTicket {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "open_ticket".into(),
            description: "Open a support ticket for the current customer.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "subject": { "type": "string" } }
            }),
        }
    }
    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<String> {
        // your side effect here — scope it to ctx.org_id / ctx.access
        Ok(format!("ticket opened: {args}"))
    }
}

struct MyTools;

#[async_trait]
impl ToolProvider for MyTools {
    async fn tools_for(&self, _ctx: &ToolProviderContext) -> Vec<Arc<dyn Tool>> {
        vec![Arc::new(OpenTicket)]
    }
}

let server = LocalServer::builder()
    .tools(Arc::new(MyTools))
    .spawn()
    .await?;
```

**Or let it gain tools with no redeploy.** The server hosts [SEP extensions](https://github.com/SmooAI/smooth-operator/blob/main/docs/TOOLS.md) — out-of-process tool providers discovered at runtime and attached to the turn (their `ui/confirm` prompts bridge straight into the protocol's confirmation frames for HITL). It's gated: an extension contributes tools **only** if you name it in `SMOOTH_EXTENSIONS_ALLOW`. Nothing loads by default.

**Now declare the lines it can't cross.** Every tool — built-in, host-provided, or from an extension — flows through the same gates, so the guardrails hold no matter where a tool came from:

- **Per-agent allow-list** — an agent's `tool_config.enabledTools` restricts its turn to exactly those tools. Off the list, off the table.
- **The auth-level `ToolHook`** — a tool tagged `admin` or `end_user` is *blocked at call time* on a public agent unless the caller is verified (via the session's OTP bit or your `SessionAuthenticator` seam). The hook runs before the tool does, and fails closed.
- **Document-level ACLs** — both retrieval paths read through `StorageAdapter::knowledge_for_access`, so a document the requester isn't entitled to is dropped before it can reach the model or land in a citation.

That's what "point it at prod" costs here: not a leap of faith, a declaration. You decide what the agent can touch; the runner enforces it.

---

## Five languages, one protocol

This is the Rust server. The *same* server — same `spec/` wire protocol, same conformance corpus — exists in five languages, so you run it where your stack already lives.

| Language | Server package | Registry |
| --- | --- | --- |
| **Rust** | `smooai-smooth-operator-server` | [crates.io](https://crates.io/crates/smooai-smooth-operator-server) |
| **C# / .NET** | `SmooAI.SmoothOperator.Server` | [in-repo](https://github.com/SmooAI/smooth-operator/tree/main/dotnet/server) |
| **TypeScript** | `@smooai/smooth-operator-server` | [in-repo](https://github.com/SmooAI/smooth-operator/tree/main/typescript/server) |
| **Python** | `smooai-smooth-operator-server` | [in-repo](https://github.com/SmooAI/smooth-operator/tree/main/python/server) |
| **Go** | `github.com/SmooAI/smooth-operator/go/server` | [in-repo](https://github.com/SmooAI/smooth-operator/tree/main/go/server) |

Every native client — [TypeScript](https://www.npmjs.com/package/@smooai/smooth-operator), Go, .NET, Python, Rust — connects to any of them unmodified. All five servers run the shared [`spec/conformance/scenarios`](https://github.com/SmooAI/smooth-operator/tree/main/spec/conformance/scenarios) corpus, so identical protocol output is a *tested* guarantee, not a hope.

---

Part of the **[smooth-operator](https://github.com/SmooAI/smooth-operator)** service — Smoo AI's polyglot AI agent service. See the [repository](https://github.com/SmooAI/smooth-operator) for the architecture, protocol, storage adapters, and the eval harness. Don't want to run it yourself? **[lom.smoo.ai](https://lom.smoo.ai)** hosts it for you.

## License

MIT © 2026 Smoo AI. See [LICENSE](https://github.com/SmooAI/smooth-operator/blob/main/LICENSE).
