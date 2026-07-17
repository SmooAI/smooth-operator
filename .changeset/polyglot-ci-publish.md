---
'@smooai/smooth-operator': patch
---

Wire Changesets to drive lockstep publishing for every polyglot server artifact — npm + NuGet + PyPI + crates.io — closing the npm-only gap.

- `scripts/sync-versions.mjs` now also stamps the .NET server package (`SmooAI.SmoothOperator.Server.csproj` `<Version>`) and the PyPI server package (`python/server/pyproject.toml`), and fails loudly if any manifest anchor is missing (never publishes an out-of-lockstep set).
- New `scripts/ci-publish.mjs`: a single idempotent orchestrator that runs sync-versions first, then publishes npm → NuGet → PyPI (client + server) → crates.io, each existence-checked + skip-if-already-published, with a `DRY_RUN=1` path that packs/validates but uploads nothing. One registry's failure no longer skips the others; any hard failure exits non-zero. `ci:publish` now points at it.
- `release.yml` folds the previously-inline crates.io/PyPI steps into `ci:publish` and adds the NuGet publish token, so the whole polyglot release goes through one orchestrator.
