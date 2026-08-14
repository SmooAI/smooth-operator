---
'@smooai/smooth-operator': minor
---

Release the .NET server work that has been sitting on `main` unpublished: the file-transfer contract (#348) and server-side skill resolution (#352).

Neither shipped, and the cause was a changeset-target slip rather than anything wrong with the code. `@smooai/smooth-operator-server` is the **TypeScript** server npm package; the .NET NuGet `SmooAI.SmoothOperator.Server` takes its `<Version>` from `scripts/sync-versions.mjs`, which stamps the **lockstep anchor** `@smooai/smooth-operator` (`typescript/package.json`) onto every non-npm manifest. #348's changeset named only `@smooai/smooth-operator-server`, so release #349 bumped the TS package to 1.8.0 and left the anchor at 1.39.0 — and the .NET package was never republished. The sibling TS PR #346 got this right, naming both packages.

Net effect for consumers: `SmooAI.SmoothOperator.Server` 1.39.0 has no `TurnContext`, no directive sink, and no `images[]`/`files[]` ingest, even though `main` has had all of it since 2026-08-11. Bumping the anchor stamps the csproj and publishes the NuGet, so 1.39.0 consumers get file transfer **and** skill resolution in one release.

No source change — this is purely the release plumbing the two .NET PRs missed.
