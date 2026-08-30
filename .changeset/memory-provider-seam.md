---
"@smooai/smooth-operator": patch
---

feat(dotnet,python,ts,go): let a host supply the turn's memory — durable auto-recall off Rust (th-ebe27d)

Rust #330 put `memory_for_access` on `StorageAdapter` and had the server runner thread the
result into the engine's agent options, which is what lights up Big Smooth's durable
auto-recall. The four sibling servers never did — and the gap was invisible, because **all
five engine cores already implement `Memory` and already recall relevant entries into
context**. The capability was fully built on both ends with nothing connecting them: no
matter what store a deployment had, every turn on these servers ran without auto-recall.

Each server now takes a `MemoryProvider` (`IMemoryProvider` in C#) with one method —
`memory_for_access(access)` — resolved per turn and passed to the engine as
`AgentOptions.memory`:

| | seam | install |
|---|---|---|
| C# | `IMemoryProvider` | DI (`services.GetService<IMemoryProvider>()`) |
| Python | `MemoryProvider` | `ServerState.memory_provider` |
| TypeScript | `MemoryProvider` | `serve({ memoryProvider })` |
| Go | `MemoryProvider` | `WithMemoryProvider(...)` |

`access` is threaded exactly as it is for knowledge, so a multi-tenant host can bind memory
to the requester's org/user; single-tenant hosts — Big Smooth's daemon, the reason the seam
exists — ignore it, so each language also ships a `StaticMemoryProvider` over one store.

**Nothing changes for anyone who does not opt in.** No provider, or a provider that returns
nothing for this caller, leaves the turn byte-for-byte what it was — and that is a test, not
a claim: each language asserts the no-provider and the declining-provider paths inject
nothing, alongside the positive case and a relevance case (an unrelated message recalls
nothing, so this is not a blanket dump of every stored memory into every turn).

Five tests per language, named after their Rust counterparts in
`rust/smooth-operator-server/tests/injection_seams.rs`, all four mutation-checked — dropping
the one line that hands memory to the engine fails them.

Two notes for whoever picks this up next. The recall block's **header text is deliberately
not asserted**: the five cores currently inject three different strings for it (th-ffaeae),
so the tests assert the recalled *content* reaches the model, which is the behavior the seam
exists for. And the bundled lexical scorer counts raw token overlap with **no stopword
filter**, so in practice a single shared "the" scores a hit — worth knowing before trusting
recall precision in production.

No wire-protocol change.
