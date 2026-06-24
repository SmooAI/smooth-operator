---
"@smooai/smooth-operator": major
---

Unified 1.0.0 polyglot publish — all five language implementations now ship from one changeset at one shared version via the existing lockstep release.

- **Rust** reclaims the crate name `smooai-smooth-operator` (the predecessor standalone engine 0.13.x is superseded by `smooai-smooth-operator-core`) and publishes the full set: the reference lib plus 7 library crates (`-ingestion`, the `-adapter-*` storage/backplane adapters, and `-server`) to crates.io.
- **Python** distributions are renamed to `smooai-smooth-operator` and `smooai-smooth-operator-core` (PyPI), keeping the `smooth_operator` / `smooth_operator_core` import packages unchanged.
- **Go** is published by tag `go/v1.0.0` (subdir module `github.com/SmooAI/smooth-operator/go`).
- **npm** (`@smooai/smooth-operator`) and **NuGet** (`SmooAI.SmoothOperator.Core`) continue as before.

One changeset → one shared version → npm + NuGet + crates.io + PyPI + Go tag, all stamped by `scripts/sync-versions.mjs`.
