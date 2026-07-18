---
'@smooai/smooth-operator': patch
---

Make the two .NET Server add-on packages publishable to NuGet and bump the Core pin. `SmooAI.SmoothOperator.Server.AspNetCore` (the ASP.NET Core WebSocket host) and `SmooAI.SmoothOperator.Server.Postgres` (the durable Postgres session store) now carry NuGet packaging metadata, get their `<Version>` stamped in lockstep by `sync-versions.mjs`, and are packed + pushed by `ci-publish.mjs` alongside the base `SmooAI.SmoothOperator.Server` package — so downstream hosts can `PackageReference` them instead of vendoring the extension source. The Server package's `SmooAI.SmoothOperator.Core` pin is also bumped from 1.5.0 to the latest published 1.7.0.
