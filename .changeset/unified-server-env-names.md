---
'@smooai/smooth-operator': minor
---

Unify the server env-var contract across all five implementations behind `SMOOTH_AGENT_*`.

The five language servers read divergent names for the same settings — Go and Python
took a combined `SMOOTH_OPERATOR_BIND`, TypeScript split it into `SMOOTH_OPERATOR_HOST` +
`SMOOTH_OPERATOR_PORT`, .NET had its own `SMOOTH_DATABASE_URL` / `SMOOTH_AUTH_MODE` family
and no bind var at all (it took ASP.NET's `:5000`), and Rust read a bare `AUTH_MODE`. Every
implementation now reads the canonical `SMOOTH_AGENT_*` names that Rust, the Helm chart,
the container images and the docs already used, so switching engines no longer means
relearning the config surface.

Each host keeps its previous names as **aliases** — the canonical name wins when both are
set — so no existing deployment breaks. Go's default port also moves from `8793` to `8787`,
matching the other four processes and all five container images.

The `SMOOAI_GATEWAY_URL` / `SMOOAI_GATEWAY_KEY` pair is deliberately unchanged: it is the
wider SmooAI gateway contract shared with launchers and benches, and was already identical
in all five hosts.
