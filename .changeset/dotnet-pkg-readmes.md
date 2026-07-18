---
'@smooai/smooth-operator': patch
---

Docs: add branded, NuGet-page READMEs for `SmooAI.SmoothOperator.Server.AspNetCore`
and `SmooAI.SmoothOperator.Server.Postgres`. Each explains what the package is,
how to install and use it (real API surface — `AddSmoothOperatorServer` /
`MapSmoothOperatorWebSocket` / `ConfirmTools`; `PostgresSessionStore` /
`PostgresAclKnowledgeStore`), and cross-references the rest of the .NET family
(Core, Server, AspNetCore, Postgres, client). Wired each via `PackageReadmeFile`
so it renders on nuget.org once the packages are published.
