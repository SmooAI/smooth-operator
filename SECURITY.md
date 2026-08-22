# Security Policy

smooth-operator is an AI agent service. It runs models with tools, executes extensions, holds
conversation history, and terminates authenticated WebSocket connections — so a bug here can reach
data and systems well outside this repository. Please report suspected vulnerabilities privately.

## Reporting a vulnerability

**Email [security@smoo.ai](mailto:security@smoo.ai).**

Please do **not** open a public issue, a Discussion, or a pull request for a suspected
vulnerability. A public report is a public exploit for everyone running the affected version.

<!--
TODO(maintainers): GitHub private vulnerability reporting is DISABLED on this repo
(checked 2026-08-22: `gh api repos/SmooAI/smooth-operator/private-vulnerability-reporting`
returned {"enabled":false}). Enable it under Settings → Security → "Private vulnerability
reporting", then list https://github.com/SmooAI/smooth-operator/security/advisories/new above
as the preferred channel — it gives reporters a tracked thread and a CVE path that email does not.
-->

Include whatever you have; a partial report is better than a silent one:

- The **version** you are on, and **which language** — the Rust, TypeScript, Python, Go, or .NET
  client/server. The same logical bug is often present in one port and not the others.
- How it is **deployed** — the standalone server, the AWS Lambda adapter, a Kubernetes deploy, an
  embedded client, or the hosted service.
- Reproduction steps, a proof of concept, and the impact you believe it has.
- Whether any of it is already public.

If you would like to be credited in the advisory, say so and give us the name or handle to use.

## What to expect

smooth-operator is maintained by a small team at Smoo AI. We will acknowledge your report, tell you
whether we could reproduce it, and keep you updated while we work on a fix. We are not going to
publish an hours-and-minutes SLA we cannot staff around the clock — but silence is a failure of this
process, not an answer. If a report goes unanswered, reply on the same thread and escalate.

We ask that you give us a reasonable window to ship a fix before disclosing publicly, and we will
credit you in the advisory unless you would rather stay anonymous.

<!--
TODO(maintainers): if someone takes ownership of the security@smoo.ai rota and can commit to a
concrete acknowledgement window (e.g. "within N business days"), replace the paragraph above with
that number. It was left unquantified deliberately rather than promising a response time nobody
had agreed to.
-->

## Supported versions

Only the **latest published release** receives security fixes.

Every published artifact — npm, NuGet, PyPI, crates.io, and the Go module — ships at one lockstep
version, so "latest" is the same number in every language. There are no maintained release
branches; fixes land on `main` and go out in the next release.

## Scope

In scope: the servers (`rust/`, `typescript/server`, `python/server`, `go/server`, `dotnet/server`),
the clients, the adapters under `adapters/`, the extension SDK and its sandboxing, and the wire
protocol in `spec/`.

Also in scope, and worth calling out for an agent framework specifically: prompt-injection paths
that cross a trust boundary (untrusted content reaching a tool call with real authority), extension
sandbox escapes, tool-permission or deny-policy bypasses, authentication and session-isolation
failures on the WebSocket protocol, and anything that leaks one tenant's conversation data to
another.

Out of scope: findings that require an already-compromised host or an already-leaked credential,
missing hardening with no demonstrated impact, and results from automated scanners submitted without
a working proof of concept. Vulnerabilities in the hosted Smoo AI platform (rather than this code)
also go to security@smoo.ai — same address, we will route it.
