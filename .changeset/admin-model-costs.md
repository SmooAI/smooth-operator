---
"@smooai/smooth-operator": patch
---

feat(go,ts,python): `GET /admin/model-costs`, so the console's cost badges render

One of the two Rust-only admin routes (epic th-9e792d item R). The Go,
TypeScript and Python servers now serve it on the same contract as
`rust/smooth-operator-server/src/admin.rs`.

Ungated, as in Rust — gateway pricing is not org-sensitive and the badges must
render on a tokenless local connection. The gateway's `/model/info` is fetched at
most once per process (pricing is stable), and **only a success is cached**: any
gateway or transport failure degrades to `{}` with a 200 and leaves the cache
unset so the next request retries. A missing badge beats a broken page, and one
blip must not pin an empty map for the process lifetime.

Field mapping mirrors Rust's `map_model_info`: entries without a `model_name` are
skipped, and every field is **null when the gateway omits it** rather than
defaulted — a $0 default would render a free-model badge on a paid model.

`POST /admin/publish` is deliberately not in this PR: the backplanes differ per
server (Go routes by connection id only, TypeScript has sinks but no publish,
Python holds no sink at all), so a faithful port needs a decision on what
`delivered` may report rather than silently returning 0.
