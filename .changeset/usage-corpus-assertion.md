---
"@smooai/smooth-operator": patch
---

test: assert `eventual_response.usage` token counts in the shared scenario corpus, 5/5 servers

`basic-streaming-turn` now asserts `data.data.usage.promptTokens` = 10 and
`data.data.usage.completionTokens` = 5. All five native servers produce it, so
per-turn token reporting is a real cross-language invariant instead of a
"not yet assertable" note in the corpus README.

Three things had to land first, two of them upstream:

- the five mock providers agree on scripted usage (`smooth-operator-core`, released);
- the Rust and C# scenario runners compare JSON numbers by value (#381);
- **the Rust runner must push a `StreamEvent::Usage(scripted_usage())` into the
  stream it synthesizes.** The sibling mocks build their own final usage chunk
  from the FIFO script; Rust's `MockLlmClient` takes an explicit event list, and
  without that event the engine falls back to estimating completion tokens from
  the reply's *length* (~4 chars/token) — the `0/5` that moved whenever a
  scenario's text changed. Verified load-bearing: removing the event fails the
  scenario with `data.data.usage.promptTokens = 0 != 10`.

The Rust and Go servers' core dependency is bumped to a release carrying the
aligned mocks (the other three already floated past it). Note that the registries
number this package independently — the release wave that published the aligned
mocks was crates.io 1.8.0 and npm/PyPI/NuGet 1.7.15 — so a cross-repo bump needs
the version read out of each ecosystem's lockfile, not one number copied across.

`costUsd` is deliberately left unasserted — it depends on which model a server
names and which pricing table its engine carries, and those legitimately differ.
The corpus README now says so explicitly rather than filing it as a gap.
