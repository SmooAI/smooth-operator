<p align="center">
  <a href="https://smoo.ai"><img src="https://raw.githubusercontent.com/SmooAI/smooth-operator/main/.github/banner-dotnet.png" alt="SmooAI.SmoothOperator.Server.Postgres — durable Postgres storage for the smooth-operator server." width="100%" /></a>
</p>

<p align="center">
  <a href="https://smoo.ai"><img src="https://img.shields.io/badge/Smoo_AI-platform-00A6A6?style=for-the-badge&labelColor=020618" alt="Smoo AI"></a>
  <a href="https://github.com/SmooAI/smooth-operator/blob/main/LICENSE"><img src="https://img.shields.io/badge/license-MIT-F49F0A?style=for-the-badge&labelColor=020618" alt="license"></a>
  <a href="https://lom.smoo.ai"><img src="https://img.shields.io/badge/hosted-lom.smoo.ai-FF6B6C?style=for-the-badge&labelColor=020618" alt="lom.smoo.ai"></a>
  <a href="https://www.nuget.org/packages/SmooAI.SmoothOperator.Server.Postgres"><img src="https://img.shields.io/nuget/v/SmooAI.SmoothOperator.Server.Postgres?style=for-the-badge&labelColor=020618&color=00A6A6" alt="NuGet"></a>
  <a href="https://dotnet.microsoft.com"><img src="https://img.shields.io/badge/.NET-8.0-00A6A6?style=for-the-badge&labelColor=020618" alt=".NET 8.0"></a>
</p>

<p align="center">
  <b><code>SmooAI.SmoothOperator.Server.Postgres</code></b> — durable, Postgres-backed storage for <a href="https://www.nuget.org/packages/SmooAI.SmoothOperator.Server"><code>SmooAI.SmoothOperator.Server</code></a>.<br/>Sessions, conversation history, checkpoints, and ACL-scoped pgvector knowledge that <b>survive a restart</b>.
</p>

---

## What is this?

`SmooAI.SmoothOperator.Server` ships with in-memory stores — perfect for tests, gone on restart. **This package makes them durable.** It provides Postgres implementations of the server's storage seams, so a real deployment keeps its sessions, message history, and retrieval index in a database instead of process memory.

It's the C# analog of the Rust [`adapters/postgres`](https://github.com/SmooAI/smooth-operator/tree/main/rust/adapters) OLTP + pgvector surface.

---

## Install

```bash
dotnet add package SmooAI.SmoothOperator.Server.Postgres
```

Targets `net8.0`. Pulls in [`Npgsql`](https://www.nuget.org/packages/Npgsql) and [`Pgvector`](https://www.nuget.org/packages/Pgvector) (the `Pgvector.Npgsql` type mapping for the knowledge adapter). Requires a Postgres with the [`pgvector`](https://github.com/pgvector/pgvector) extension for `PostgresAclKnowledgeStore`.

---

## What ships in this package

| Type | Implements | Purpose |
|---|---|---|
| `PostgresSessionStore` | `ISessionStore` | Sessions + conversation history that survive a restart. |
| `PostgresAclKnowledgeStore` | `IAclKnowledge` | ACL-scoped RAG over `pgvector` — grounding filtered by access context. |
| `PostgresCheckpointStore` | `ICheckpointStore` | Durable agent checkpoints (resume mid-turn). |
| `PostgresKnowledgeBase` | `IKnowledgeBase` | Postgres-backed knowledge base. |

Each is created by an async factory (they open + verify the connection and ensure schema) and registered against the interface the server resolves.

---

## Quickstart — make the server durable

Register the Postgres stores against the interfaces [`SmooAI.SmoothOperator.Server`](https://www.nuget.org/packages/SmooAI.SmoothOperator.Server) resolves, and the [`.AspNetCore`](https://www.nuget.org/packages/SmooAI.SmoothOperator.Server.AspNetCore) host picks them up instead of the in-memory defaults:

```csharp
using SmooAI.SmoothOperator.Server;
using SmooAI.SmoothOperator.Server.AspNetCore;
using SmooAI.SmoothOperator.Server.Postgres;

var pg = builder.Configuration.GetConnectionString("SmoothOperator")!;

// Durable sessions + conversation history.
builder.Services.AddSingleton<ISessionStore>(
    _ => PostgresSessionStore.CreateAsync(pg).GetAwaiter().GetResult());

// ACL-scoped pgvector RAG (needs an IEmbedder).
builder.Services.AddSingleton<IAclKnowledge>(sp =>
    PostgresAclKnowledgeStore.CreateAsync(pg, sp.GetRequiredService<IEmbedder>())
        .GetAwaiter().GetResult());

builder.Services.AddSmoothOperatorServer();          // resolves the durable stores above
```

> Prefer async initialization at host startup over the `GetAwaiter().GetResult()` shorthand shown here — e.g. build the stores in an `IHostedService` / `await`-ing startup path and register the resolved instances.

---

## Related packages

One protocol, one .NET family — each package is published on NuGet and references the others:

| Package | Role |
|---|---|
| [`SmooAI.SmoothOperator.Core`](https://www.nuget.org/packages/SmooAI.SmoothOperator.Core) | The agent **engine** — `SmoothAgent`, tools, checkpoints. |
| [`SmooAI.SmoothOperator.Server`](https://www.nuget.org/packages/SmooAI.SmoothOperator.Server) | The agent **server loop** — sessions, streaming turns, dispatch, HITL. |
| [`SmooAI.SmoothOperator.Server.AspNetCore`](https://www.nuget.org/packages/SmooAI.SmoothOperator.Server.AspNetCore) | The ASP.NET Core WebSocket host. |
| **`SmooAI.SmoothOperator.Server.Postgres`** | **This package** — durable Postgres stores. |
| [`SmooAI.SmoothOperator`](https://www.nuget.org/packages/SmooAI.SmoothOperator) | The native .NET **client** (with an `IChatClient` facade). |

---

## 🧩 Part of Smoo AI

Built and open-sourced by **[Smoo AI](https://smoo.ai)** — the AI-powered business platform with AI built into every product.

- 🌐 **The service** — [smooth-operator](https://github.com/SmooAI/smooth-operator) (protocol, servers, the five clients, AWS/k8s deploy)
- 🛰️ **Protocol** — [`docs/PROTOCOL.md`](https://github.com/SmooAI/smooth-operator/blob/main/docs/PROTOCOL.md)
- ☁️ **Hosted** — [lom.smoo.ai](https://lom.smoo.ai) runs smooth-operator for you, managed and multi-tenant
- 💬 **Issues** — [github.com/SmooAI/smooth-operator/issues](https://github.com/SmooAI/smooth-operator/issues)

## 📄 License

MIT © 2026 Smoo AI. See [LICENSE](https://github.com/SmooAI/smooth-operator/blob/main/LICENSE).

---

<p align="center">
  Built by <a href="https://smoo.ai"><strong>Smoo AI</strong></a> — AI built into every product.
</p>
