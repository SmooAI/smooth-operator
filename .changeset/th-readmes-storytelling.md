---
'@smooai/smooth-operator': patch
---

Docs: elevate the server + registry-landing READMEs into a narrative story. Root
README gets a sharper problem→vision hook, a "safe by construction" section
(ToolHook auth-gate + per-agent allow-list + document ACLs + SEP allowlist), and
a clean language→client→server→registry table. Each per-language server README
(Rust crates.io crate, TypeScript, Python, Go, .NET) now leads with a hook, a
"spin up a real agent server in N lines" snippet, an honest "extending via
tools + guardrails" example in that language's real API, badges, and the polyglot
table. No code changes; accuracy verified against the shipped surface.
