---
"@smooai/smooth-operator": patch
---

fix(ts,python,dotnet): make `eventual_response.usage.costUsd` real instead of always 0

The TypeScript, Python and .NET servers injected a raw SDK client into the engine —
`openai`'s `OpenAI`/`AsyncOpenAI`, and MEAI's OpenAI adapter. Every one of those
parses the HTTP response and throws the headers away, and the gateway reports
per-request cost ONLY in a response header. So core's cost-header parser (shipped
across all five engines in core#121) had nothing to read, and every turn on these
three servers reported `costUsd: 0`. Go was already correct because it injects core's
own `GatewayClient`; Rust because it builds its own `LlmClient`.

All three now inject the header-reading client core ships:

- **TypeScript** — `createGatewayClient({ baseURL, apiKey })`. This also deletes the
  optional lazy-`import('openai')` dance, its swallow-everything `catch`, and a
  hand-rolled `createStream` adapter that had no way to carry a cost at all. `openai`
  now arrives transitively through core.
- **Python** — `GatewayLlmProvider(client=…)`, wrapping the `AsyncOpenAI` the server
  already built so the base-url-optional branch stays the single place that decides
  the endpoint.
- **.NET** — `GatewayChatClient`, which also drops the `Microsoft.Extensions.AI.OpenAI`
  package reference from the host entirely.

.NET needed a second fix: its `TurnRunner` hardcoded `new TurnUsage(0, …)`, so the
client swap alone would have changed nothing. It now folds the gateway cost the
client surfaces on each streaming update's `AdditionalProperties`, summed across the
turn's model calls. The `ponytail:` comment that documented the old always-zero
behaviour is updated rather than left to mislead.

Absent-and-zero handling is unchanged and now tested at the server boundary: a header
that is PRESENT and reports `0` is not locked in as a real $0 — it falls through
exactly as an absent header does.

Tests are real turns against a real local HTTP+SSE gateway in each language, asserted
on what the protocol actually emits rather than inside the engine — TS 5, Python 5,
.NET 5. Each language additionally pins the WIRING (that the server hands the engine a
header-reading client), because the pipeline tests inject the client directly and
would stay green if the server regressed to the raw SDK. Both halves were
mutation-tested: reintroducing the bug fails exactly the intended tests.

Core minimums move to the lowest version per registry that actually ships the clients:
npm `^1.8.4`, PyPI `>=1.8.3`, NuGet `1.7.16` (NuGet has no 1.8.x; PyPI's 1.8.4 has no
installable files).
