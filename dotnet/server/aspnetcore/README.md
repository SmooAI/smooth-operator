<p align="center">
  <a href="https://smoo.ai"><img src="https://raw.githubusercontent.com/SmooAI/smooth-operator/main/.github/banner-dotnet.png" alt="SmooAI.SmoothOperator.Server.AspNetCore — the ASP.NET Core WebSocket host for the smooth-operator server." width="100%" /></a>
</p>

<p align="center">
  <a href="https://smoo.ai"><img src="https://img.shields.io/badge/Smoo_AI-platform-00A6A6?style=for-the-badge&labelColor=020618" alt="Smoo AI"></a>
  <a href="https://github.com/SmooAI/smooth-operator/blob/main/LICENSE"><img src="https://img.shields.io/badge/license-MIT-F49F0A?style=for-the-badge&labelColor=020618" alt="license"></a>
  <a href="https://lom.smoo.ai"><img src="https://img.shields.io/badge/hosted-lom.smoo.ai-FF6B6C?style=for-the-badge&labelColor=020618" alt="lom.smoo.ai"></a>
  <a href="https://www.nuget.org/packages/SmooAI.SmoothOperator.Server.AspNetCore"><img src="https://img.shields.io/nuget/v/SmooAI.SmoothOperator.Server.AspNetCore?style=for-the-badge&labelColor=020618&color=00A6A6" alt="NuGet"></a>
  <a href="https://dotnet.microsoft.com"><img src="https://img.shields.io/badge/.NET-8.0-00A6A6?style=for-the-badge&labelColor=020618" alt=".NET 8.0"></a>
</p>

<p align="center">
  <b><code>SmooAI.SmoothOperator.Server.AspNetCore</code></b> — the ASP.NET Core host that turns <a href="https://www.nuget.org/packages/SmooAI.SmoothOperator.Server"><code>SmooAI.SmoothOperator.Server</code></a> into a running WebSocket backend.<br/>The <a href="https://github.com/SmooAI/smooth-operator/blob/main/docs/PROTOCOL.md">smooth-operator</a> protocol on a <code>/ws</code> endpoint, wired through DI, in a handful of lines.
</p>

---

## What is this?

`SmooAI.SmoothOperator.Server` gives you the agent loop — sessions, streaming turns, tool-calling, HITL, citations. **This package is its deployable surface**: the ASP.NET Core glue that maps that loop onto a real WebSocket endpoint and registers it in the DI container. If `.Server` is the engine, `.Server.AspNetCore` is the chassis you actually drive.

It's the C# analog of the [Rust](https://github.com/SmooAI/smooth-operator/blob/main/rust/smooth-operator-server), [Go](https://github.com/SmooAI/smooth-operator/tree/main/go/server), [TypeScript](https://github.com/SmooAI/smooth-operator/tree/main/typescript/server), and [Python](https://github.com/SmooAI/smooth-operator/tree/main/python/server) server hosts — all speaking one schema-driven protocol from [`spec/`](https://github.com/SmooAI/smooth-operator/tree/main/spec).

---

## Install

```bash
dotnet add package SmooAI.SmoothOperator.Server.AspNetCore
```

Targets `net8.0`. References `Microsoft.AspNetCore.App` (framework) and brings in [`SmooAI.SmoothOperator.Server`](https://www.nuget.org/packages/SmooAI.SmoothOperator.Server) — which in turn pulls the [`SmooAI.SmoothOperator.Core`](https://www.nuget.org/packages/SmooAI.SmoothOperator.Core) engine.

---

## Quickstart — a full agent backend in one `Program.cs`

```csharp
using Microsoft.Extensions.AI;
using SmooAI.SmoothOperator.Server;
using SmooAI.SmoothOperator.Server.AspNetCore;

var builder = WebApplication.CreateBuilder(args);

builder.Services.AddSingleton<IChatClient>(/* your model, e.g. an OpenAI/Anthropic IChatClient */);
builder.Services.AddSmoothOperatorServer();          // session store + turn runner + frame dispatcher

var app = builder.Build();
app.MapSmoothOperatorWebSocket("/ws");               // the protocol endpoint
app.Run();
```

That's it — sessions, streaming turns, tool-calling, and citations on one WebSocket. Any of the [five native clients](https://github.com/SmooAI/smooth-operator) (including [`SmooAI.SmoothOperator`](https://www.nuget.org/packages/SmooAI.SmoothOperator) for .NET) can connect and drive a turn.

### Gate the dangerous tools (HITL)

Register a `ConfirmTools` and matching tool calls pause for human approval — the resumed stream flows back into the same turn:

```csharp
builder.Services.AddSingleton(new ConfirmTools("delete_record", "send_email"));
```

### Admin endpoints

```csharp
app.MapSmoothOperatorAdmin("/admin");                // optional operational surface
```

---

## What ships in this package

| API | What it does |
|---|---|
| `IServiceCollection.AddSmoothOperatorServer()` | Registers the store, turn runner, and dispatcher (defaults to the in-memory session store — swap in [`SmooAI.SmoothOperator.Server.Postgres`](https://www.nuget.org/packages/SmooAI.SmoothOperator.Server.Postgres) for durability). |
| `WebApplication.MapSmoothOperatorWebSocket(path, dispatcherFor?)` | Maps the smooth-operator protocol onto a WebSocket endpoint. |
| `IEndpointRouteBuilder.MapSmoothOperatorAdmin(prefix)` | Optional admin/operational routes. |

---

## Related packages

One protocol, one .NET family — each package is published on NuGet and references the others:

| Package | Role |
|---|---|
| [`SmooAI.SmoothOperator.Core`](https://www.nuget.org/packages/SmooAI.SmoothOperator.Core) | The agent **engine** — `SmoothAgent`, tools, checkpoints. |
| [`SmooAI.SmoothOperator.Server`](https://www.nuget.org/packages/SmooAI.SmoothOperator.Server) | The agent **server loop** — sessions, streaming turns, dispatch, HITL. |
| **`SmooAI.SmoothOperator.Server.AspNetCore`** | **This package** — the ASP.NET Core WebSocket host. |
| [`SmooAI.SmoothOperator.Server.Postgres`](https://www.nuget.org/packages/SmooAI.SmoothOperator.Server.Postgres) | Durable Postgres stores — sessions + ACL-scoped pgvector knowledge. |
| [`SmooAI.SmoothOperator`](https://www.nuget.org/packages/SmooAI.SmoothOperator) | The native .NET **client** (with an `IChatClient` facade) for talking to this server. |

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
