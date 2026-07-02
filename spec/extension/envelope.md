# SEP envelope & framing

SEP (Smooth Extension Protocol) is **JSON-RPC 2.0**, one message per line
(ndjson), over an extension subprocess's **stdin/stdout**. stderr is not part of
the protocol — the host forwards it to its own tracing. This is byte-for-byte
the framing MCP stdio uses (rmcp precedent in `smooth-operative/src/mcp.rs`), so
a live session is debuggable with `jq`.

## The wire

Every message is a single line of UTF-8 JSON terminated by `\n`. There are four
JSON-RPC frame shapes, all carrying `"jsonrpc": "2.0"`:

| Frame | Has `id` | Has `method` | Payload |
|---|---|---|---|
| **Request** | yes | yes | `params` (object, optional) — a reply is expected |
| **Notification** | no | yes | `params` (object, optional) — fire-and-forget |
| **Response (ok)** | yes (echoes request) | no | `result` |
| **Response (err)** | yes (echoes request) | no | `error: { code, message, data? }` |

`id` is an integer or string, unique per sender per connection. Because the
channel is symmetric, **both** peers issue requests; each numbers its own `id`
space. `method` names are namespaced with `/` (`tool/execute`, `session/send_message`)
and the JSON-RPC-reserved `$/` prefix marks meta methods (`$/cancel`). All
`params`/`result` field names are `snake_case`.

The schema for the four frame shapes is [`methods/envelope.schema.json`](./methods/envelope.schema.json).
Per-method `params`/`result` schemas live one file each under `methods/`.

## Lifecycle

```
host spawns child ─▶ host → ext  initialize (request)
                     ext → host  initialize result   (capabilities + registrations)
                     … steady state: events, hooks, tool/execute, ui/*, kv/*, … …
host → ext  shutdown (request) ─▶ ext replies, host waits ≤5s, then SIGKILL
```

The `initialize` **result** carries the extension's registrations (tools,
commands, flags, event subscriptions). Registrations may also be updated later
via the `registry/update` notification.

## Method catalog

**Host → extension**

| Method | Kind | Purpose |
|---|---|---|
| `initialize` | request | handshake: negotiate `protocol_version`, exchange capabilities, collect registrations |
| `shutdown` | request | graceful stop (5s grace, then SIGKILL) |
| `ping` | request | health probe (either peer may send) |
| `event` | notification | observe: fire-and-forget lifecycle/turn event |
| `hook` | request | intercept: awaited, returns a `HookOutcome` |
| `tool/execute` | request | run a tool the extension registered; streams `tool/update` back, cancellable via `$/cancel` |
| `command/execute` | request | run a slash-command the extension registered |
| `$/cancel` | notification | cancel an in-flight request by `id` |

**Extension → host**

| Method | Kind | Purpose |
|---|---|---|
| `ping` | request | health probe (either peer may send) |
| `registry/update` | notification | add/replace registrations after handshake |
| `tools/set_active` | request | set the active tool subset (clamped to per-agent `enabled_tools` — never widens) |
| `tool/update` | notification | progress for an in-flight `tool/execute` |
| `session/send_message` | request | post an assistant/user message into the session |
| `session/append_entry` | request | append a transcript entry |
| `session/set_model` | request | switch the model for the session |
| `session/state` | request | read session state (awaited) |
| `exec/run` | request | run a command through the host's audited permission engine |
| `ui/request` | request | ask the frontend for `select`/`confirm`/`input`/`notify`/`set_status`/`set_widget`/`set_title` |
| `kv/get` · `kv/set` · `kv/delete` · `kv/list` | request | host-pluggable key/value store (Dolt on the daemon, JSON file on the CLI) |
| `bus/publish` | notification | inter-extension event bus |
| `log` | notification | structured log line, folded into host tracing |

## Context tiers (deadlock guard)

Every dispatched `event`/`hook`/`tool/execute`/`command/execute` carries
`context: { token, tier }` where `tier` is `"event"` or `"command"`. Actions
that mutate the session (`session/*`, `session/set_model`, …) require the
**command** tier; attempting them from an event-tier context is rejected with
`-32003 ContextViolation`. The `token` is an opaque epoch handle — the host
invalidates stale tokens across reloads so a slow extension can't act on a
context that no longer exists.

## Hooks

Hooks are intercepts, chained across extensions **in load order**; each sees the
prior extension's patch, and the host folds the chain. Every hook reply is a
`HookOutcome` (see [`methods/hook.schema.json`](./methods/hook.schema.json)):

```json
{ "action": "continue" }                         // proceed unchanged
{ "action": "block", "reason": "rm -rf blocked" } // veto
{ "action": "modify", "patch": { … } }            // mutate the intercepted value
```

On timeout or extension crash the host applies the hook's **default**:

| Hook | Default on timeout/crash | Timeout |
|---|---|---|
| `tool_call` | **fail-closed** (block) | 60s |
| `user_bash` | **fail-closed** (block) | 60s |
| `tool_result`, `input`, `before_agent_start`, `context`, `before_provider_request`, `message_end`, `session_before_*` | **fail-open** (continue) | 5s |

Timeouts are manifest-overridable via `hook_timeout_ms`.

## Error codes

Standard JSON-RPC plus the SEP range:

| Code | Name | Meaning |
|---|---|---|
| -32700 | ParseError | malformed JSON line |
| -32600 | InvalidRequest | not a valid JSON-RPC frame |
| -32601 | MethodNotFound | unknown `method` |
| -32602 | InvalidParams | `params` failed schema validation |
| -32603 | InternalError | unhandled host/extension error |
| **-32000** | **Blocked** | a hook or policy vetoed the operation |
| **-32001** | **NoUI** | `ui/request` in a headless/uncapable frontend |
| **-32002** | **NotTrusted** | extension acted beyond its granted trust |
| **-32003** | **ContextViolation** | command-tier action attempted from an event-tier context |
| **-32004** | **CapabilityDisabled** | method requires a capability the handshake did not enable |
| **-32800** | **Cancelled** | request cancelled via `$/cancel` |

## WebSocket binding (specified, deferred)

A remote extension MAY be reached over WebSocket instead of stdio: **one JSON-RPC
message per text frame**, bearer-token auth in the `Authorization` header at
connect (mirroring `wss://` conventions elsewhere in this repo). Semantics are
identical — SEP is transport-agnostic. This binding is **not implemented in v1**:
stdio's free properties (process identity = connection identity, crash detection =
process exit) cover 100% of known demand. It is written down here so the day a
real remote-extension need arrives, the framing is already decided.
