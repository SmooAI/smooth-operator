---
"@smooai/smooth-operator": patch
---

feat(dotnet): `GET /admin/model-costs` and `POST /admin/publish`, closing .NET to full admin parity

The two admin routes that landed for the other engines while the .NET server was
catching up on the console surface. `model-costs` went to Go, TypeScript and Python;
`publish` went to Go and TypeScript. .NET had neither, which left it the only engine
missing `model-costs` and one of two missing `publish`. With these it serves the whole
shared admin surface.

**`GET /admin/model-costs`** is ungated, exactly as in Rust: gateway pricing is not
org-sensitive and cost badges must render on a tokenless local connection. It maps the
gateway's `/model/info` into `{ "<model>": { inputCostPerToken, outputCostPerToken,
tier, useCases, maxOutputTokens } }` via a new pure `ModelInfo.MapModelInfo`, fetched
at most once per process. Two details are load-bearing and both are tested: an omitted
field stays **null rather than defaulted**, because a `0` cost would render a
free-model badge on a paid model; and only a **success** is cached, because caching a
failure would pin an empty map for the life of the process and leave every badge
missing until a restart even after the gateway recovered.

**`POST /admin/publish`** pushes a realtime event to a target over a new `IBackplane`
connection registry — the plug point for non-AI publishers (job status, ingestion
progress, notifications) that need to reach a connected client without going through an
agent turn. Admin-gated. `connection` targets deliver for real and report a truthful
`delivered` count of 0 or 1 taken from the sink registry. `session` / `user` / `org` /
`agent` answer a hard **501 `UNSUPPORTED_TARGET` with no `delivered` field at all**: a
connection-id registry cannot route them, and `{"delivered": 0}` would let a caller
read "accepted, reached nobody" as success for an event that was never routable. When
the cross-pod fan-out lands, each target flips from a 501 to a real count.

`IBackplane` + `InMemoryBackplane` are the first backplane in `dotnet/` — the other
engines each had one and .NET did not. Ported from the Go shape and deliberately
synchronous: every operation is a dictionary access plus a channel write, and the
TS/Python `attach`/`detach` are async only because those ecosystems default to it. The
WebSocket host attaches each connection's outbound channel as its sink and detaches on
teardown; the detach is the half that matters, since a leaked sink would report
`delivered: 1` into a channel whose socket is long gone, and it is covered by a test
that drives a real WebSocket and asserts the registry empties.

Also fixes a latent crash in the existing `ModelInfo.ParseCeilings`, found while
writing the mapper's malformed-payload tests: indexing a `JsonNode` that is not an
object **throws**, so a gateway payload carrying a scalar `model_info` (or a scalar
entry, or a non-string `model_name`) took the parse down instead of reading as "no
ceiling". It was contained only by `FetchCeilingAsync`'s catch-all; the pure function is
public and threw. Both parsers now coerce at every level.

One .NET-specific quirk worth recording: a top-level `JsonObject` handed to
`Results.Ok` serializes to an **empty body**. It round-trips correctly as a property —
which is why a connector's `config` was fine and nothing caught this earlier — so
`model-costs` writes `ToJsonString()` directly. Left as `Results.Ok`, the route would
have returned a permanently empty `200`, indistinguishable from "the gateway is down".

Verified by 14 new tests (3 pure mapper cases, 1 sequential route test covering
ungated + degrade-to-empty + cache-only-success against a real stub gateway, 8 publish
cases including the four unroutable targets, 1 WebSocket attach/detach lifecycle, plus
the fail-closed table row) and by exercising all 16 route combos against a booted C#
host: no 404s, no 5xx.
