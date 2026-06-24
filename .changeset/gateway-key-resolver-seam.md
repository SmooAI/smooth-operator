---
"@smooai/smooth-operator": minor
---

Add a per-org LLM gateway-key resolution seam so a multi-tenant flavor can
bill/scope each org's turns to its own gateway key (e.g. a per-tenant LiteLLM
virtual key), while the local/default flavor keeps using the single environment
key.

- New `GatewayKeyResolver` trait (`smooth_operator::gateway_key`) — the public,
  contributable hook: `async fn resolve(&self, org_id: &str) -> Option<String>`.
- Default `EnvGatewayKeyResolver` returns the single `SMOOAI_GATEWAY_KEY` for
  every org, so behavior is unchanged unless a host injects a per-org resolver.
- `resolve_gateway_key(resolver, org_id, env_key)` helper centralizes the
  resolve-then-fall-back-to-env contract used by the per-turn LLM-config build.
- The server's `AppState` holds an `Arc<dyn GatewayKeyResolver>` (default =
  `EnvGatewayKeyResolver`) with a `with_gateway_key_resolver(...)` builder for
  injection. `send_message` resolves the turn's `org_id` from its conversation,
  resolves the key, and falls back to the env key when the resolver returns
  `None`.

Behavior-preserving by default: with no resolver injected, every turn uses the
env key exactly as before. No SmooAI/DB specifics live in the shared code — only
the trait and the env default; a host injects its own per-org key store.
