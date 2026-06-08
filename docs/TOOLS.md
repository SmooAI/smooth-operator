# Built-in tools

The reference core ships a small **built-in tool catalog** the agent can call
during a turn. Each tool implements smooth-operator's `Tool` trait, so the
engine's `Agent` invokes it like any other tool. The catalog lives in
`rust/smooth-operator/src/tools/`.

| Tool | Args | Returns | Read-only | Source |
| --- | --- | --- | --- | --- |
| `knowledge_search` | `{ "query": string, "limit"?: 1–10 }` | Top-K KB snippets (source, id, relevance) | ✅ | `tools/knowledge_search.rs` |
| `conversation_history` | `{ "limit"?: 1–100 }` | Recent messages of the **current** conversation, oldest-first | ✅ | `tools/conversation_history.rs` |
| `fetch_url` | `{ "url": string }` | Readable text of a **public** page (HTML→text, length-capped) | ✅ | `tools/fetch_url.rs` |
| `web_search` | `{ "query": string, "limit"?: 1–10 }` | Search hits via a pluggable provider (Noop by default) | ✅ | `tools/web_search.rs` |

## The `Tool` shape

A tool is anything that implements smooth-operator's `Tool` trait:

```rust
use async_trait::async_trait;
use smooth_operator_core::tool::ToolSchema;
use smooth_operator_core::Tool;

pub struct MyTool;

#[async_trait]
impl Tool for MyTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "my_tool".to_string(),
            description: "What it does and WHEN the model should call it.".to_string(),
            // JSON Schema for the arguments object.
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "arg": { "type": "string", "description": "..." }
                },
                "required": ["arg"]
            }),
        }
    }

    async fn execute(&self, arguments: serde_json::Value) -> anyhow::Result<String> {
        let arg = arguments
            .get("arg")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("my_tool requires a string 'arg'"))?;
        Ok(format!("did the thing with {arg}"))
    }

    // Override when the tool only reads (no side effects). Default: false.
    fn is_read_only(&self) -> bool { true }
}
```

- `schema()` is what the model sees. The `description` is the model's only
  guidance on *when* to call the tool — write it for the model, not for you.
- `execute()` receives the parsed JSON arguments and returns a `String` the
  model reads on the next turn. Validate inputs and return a descriptive error
  (don't `unwrap`) — the engine surfaces the error text back to the model.
- All four built-ins are `is_read_only() == true`; they never mutate state.

## Assembling the catalog: `builtin_tools(ctx)`

`builtin_tools` assembles the whole catalog from a single `ToolContext`:

```rust
use std::sync::Arc;
use smooth_operator_core::ToolRegistry;
use smooth_operator::tools::{builtin_tools, ToolContext};
use smooth_operator::adapter::StorageAdapter;

fn wire(storage: Arc<dyn StorageAdapter>) {
    let ctx = ToolContext::new(storage, "conversation-123");

    let mut tools = ToolRegistry::new();
    for tool in builtin_tools(&ctx) {
        tools.register(tool);
    }
    // hand `tools` to Agent::new(config, tools)
}
```

`ToolContext` bundles what the broader catalog needs:

- `storage: Arc<dyn StorageAdapter>` — `conversation_history` reads the message
  log from it.
- `conversation_id: String` — the conversation the tools are scoped to.
  `conversation_history` can only read **this** conversation; the model cannot
  pass an arbitrary id.
- `web_search: Arc<dyn WebSearchProvider>` — the search backend. Defaults to
  `NoopWebSearchProvider`.

> The reference `KnowledgeChatRuntime` registers only `knowledge_search` today.
> `builtin_tools` is the opt-in path for a runtime/service that wants the full
> catalog — register them on the `ToolRegistry` you hand to `Agent::new`.

### `github_search` — live GitHub lookups (registered separately)

`GithubSearchTool` does **live** GitHub code/issue search — fresh lookups beyond
the indexed knowledge snapshot. It is **not** part of `builtin_tools` because it
needs an explicit `GithubAuth` + a default `owner/repo` scope; register it
yourself:

```rust
use smooth_operator::tools::github_search::{GithubAuth, GithubSearchTool};

tools.register(Box::new(GithubSearchTool::new(
    GithubAuth::Token(token), "smooai", "smooth-operator",
)));
```

Arguments: `{ "query": string, "kind"?: "code" | "issues" }`. The live network
sits behind a pluggable `GithubSearchBackend` (default `OctocrabGithubSearch`),
so arg-parsing + formatting are unit-tested offline and the live path is
`SMOOTH_AGENT_E2E`-gated — same split as `web_search`. Full reference:
[CONNECTORS.md](CONNECTORS.md#the-github_search-tool-live-fresh-lookups).

## `fetch_url` and the SSRF guard

`fetch_url` is an agent-controllable URL fetcher — a classic
**Server-Side Request Forgery (SSRF)** vector. A crafted URL could otherwise
reach cloud metadata (`http://169.254.169.254/…`), `localhost`, or internal
private-range services.

Before any request is built, `assert_url_is_public(url)` rejects:

- non-`http`/`https` schemes (`file://`, `ftp://`, `gopher://`, …);
- `localhost` and any `*.localhost`;
- loopback IPs (`127.0.0.0/8`, `::1`);
- link-local IPs (`169.254.0.0/16`) — **this covers the cloud metadata IP**;
- private ranges (`10/8`, `172.16/12`, `192.168/16`), unspecified (`0.0.0.0`),
  broadcast, documentation ranges, and carrier-grade NAT (`100.64.0.0/10`);
- IPv6 loopback / unspecified / unique-local (`fc00::/7`) / link-local
  (`fe80::/10`), and IPv4-mapped IPv6 (`::ffff:a.b.c.d`) that maps to a blocked
  IPv4.

The guard runs on the **parsed literal host before the request is sent**, so a
rejected URL is never fetched.

**Known limitation — DNS rebinding.** The guard validates the *literal* host. A
hostname that resolves via DNS to a private IP is not caught here. For
production, additionally pin/validate the resolved address or front the fetcher
with an egress proxy.

The response body is best-effort HTML→text (drops `<script>`/`<style>` and
comments, strips tags, decodes common entities, collapses whitespace) and capped
at 8,000 characters.

## Plugging in a web-search provider

`web_search` is intentionally provider-agnostic — we do **not** hardcode a paid
API. Implement the `WebSearchProvider` trait over your provider (Brave, Bing,
Tavily, SerpAPI, …) and inject it via `ToolContext::with_web_search`:

```rust
use std::sync::Arc;
use async_trait::async_trait;
use smooth_operator::tools::{SearchResult, ToolContext, WebSearchProvider};
use smooth_operator::adapter::StorageAdapter;

struct BraveSearch { api_key: String, http: reqwest::Client }

#[async_trait]
impl WebSearchProvider for BraveSearch {
    async fn search(&self, query: &str, k: usize) -> anyhow::Result<Vec<SearchResult>> {
        // call your provider's API, map hits → SearchResult
        let hits = self.call_brave(query, k).await?;
        Ok(hits
            .into_iter()
            .map(|h| SearchResult::new(h.title, h.url, h.snippet))
            .collect())
    }

    fn name(&self) -> &str { "brave" }
}

fn wire(storage: Arc<dyn StorageAdapter>, brave: BraveSearch) -> ToolContext {
    ToolContext::new(storage, "conversation-123")
        .with_web_search(Arc::new(brave))
}
```

Until a provider is injected, the catalog uses `NoopWebSearchProvider`, which
returns a single explanatory result ("no web-search provider is configured")
rather than an empty list the model might mistake for "no results found".

> **Secrets:** in the SmooAI monorepo, the provider's API key comes from
> `@smooai/config` (never a raw env var or `sst.Secret`). This crate just takes
> the constructed provider; key sourcing is the caller's responsibility.

## Adding a new built-in tool

1. Add `tools/<your_tool>.rs` implementing `Tool` (follow `knowledge_search.rs`).
2. Add unit tests in the same file for any logic that needs **no** network or
   storage adapter (pure input validation, formatting, guards). Tests that need
   the in-memory adapter go in `tests/builtin_tools.rs` (an integration test) —
   a `src/` unit test that depends on the `…-adapter-memory` dev-dependency
   would pull in two copies of `smooth-operator` and the
   `StorageAdapter` trait impls wouldn't line up.
3. Export it from `tools/mod.rs` and, if it belongs in the default catalog, add
   it to `builtin_tools(ctx)`.
4. Re-export from `lib.rs` if consumers should reach it directly.
5. Update the table at the top of this doc.

## Related

- `docs/STORAGE.md` — the `StorageAdapter` seam the tools read through.
- `docs/ARCHITECTURE.md` — where tools sit in the agent turn.
- `rust/smooth-operator/src/tools/` — the implementations.
