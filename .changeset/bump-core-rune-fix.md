---
'@smooai/smooth-operator': patch
---

go/typescript-server: bump `smooth-operator-core` to 1.13.2 and retire the last `knownDivergences` marker.

Core 1.13.2 (th-6fdd1c) fixes the mock LLM provider's streamed text chunker, which split on **byte** boundaries in Go and **UTF-16 code unit** boundaries in TypeScript — cutting multi-byte characters in half and turning each fragment into `U+FFFD` once serialized.

That was the one defect still keeping the shared conformance corpus from full parity: `interaction-choices-park-resume` streams `"Pro it is — pulling that quote up."`, whose 36 bytes put a 3-way chunk boundary in the middle of the em-dash, so the Go server accumulated `"Pro it is ��� pulling that quote up."`.

With the bump, **`spec/conformance/scenarios/` now carries no `knownDivergences` marker at all** — all 18 scenarios pass on all five servers (Rust, Go, TypeScript, Python, .NET). The marker did exactly what it was designed to do: it was re-pointed at the real remaining defect rather than deleted, and CI expired it automatically the moment that defect was fixed, failing with `remove go from knownDivergences … it now passes`.

Go moves 1.8.9 → 1.13.2 and TypeScript's lockfile 1.8.6 → 1.13.2; Python and .NET were already unaffected by the chunker bug and are untouched.
