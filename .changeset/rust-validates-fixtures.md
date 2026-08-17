---
'@smooai/smooth-operator': patch
---

Rust now validates the shared conformance fixtures.

Go, TypeScript, Python and .NET all check `spec/conformance/fixtures.json` against the schemas
it declares. Rust did not — which made the reference implementation the one implementation that
could not catch a spec/code divergence. th-68897a shipped against a stale `required` list and
Rust noticed nothing; .NET failed on first contact purely because it validates.
