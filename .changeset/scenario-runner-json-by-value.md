---
"@smooai/smooth-operator": patch
---

test(rust,dotnet): compare scenario-corpus JSON numbers BY VALUE, not by representation

The Rust and C# scenario-parity runners strict-compared JSON numbers, in
**opposite** directions: `serde_json::Value`'s `PartialEq` compares a `Number`'s
internal discriminant, so Rust emitted `0.0` and rejected an integer `0`, while
`JsonNode.DeepEquals` compares representation, so C# emitted `0` and rejected
`0.0`. Go (marshal → `float64` → `DeepEqual`), Python (`0 == 0.0`) and
TypeScript (both parse to `number`) already compared loosely.

That split alone made `eventual_response.usage` unassertable in the shared
corpus — its `costUsd` is a float that a matcher naturally writes as `0` — so no
scenario could name a numeric field without picking a spelling that two of the
five runners would reject.

Both runners now compare numbers by value and recurse structurally through
arrays and objects; everything that is not a number keeps exact equality, so a
number still never equals its string. Each runner gains an offline unit test
pinning this, independent of the corpus.

Test-only: no server or protocol behavior changes.
