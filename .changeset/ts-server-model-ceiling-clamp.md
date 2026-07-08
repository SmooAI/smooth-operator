---
'@smooai/smooth-operator': patch
---

TypeScript server: model-output ceiling clamp + raised starvation-prone defaults (EPIC th-1cc9fa), matching the Rust/Python server reference.

- `typescript/server/src/modelCeiling.ts`: best-effort per-model output ceiling from the gateway's `/model/info` (`extractModelCeilings` + `createGatewayModelCeilingResolver`), cached once per process, `undefined` on any error ⇒ engine leaves `max_tokens` unclamped.
- `turnRunner.ts`: raise `DEFAULT_MAX_TOKENS` 512→8192 and `DEFAULT_MAX_ITERATIONS` 6→20 (chat-widget sizing starved reasoning models), thread the per-turn ceiling into the engine via `AgentOptions.modelMaxOutput`, and set an explicit `DEFAULT_MODEL` shared by the request and the ceiling lookup.
- Thread `model` + `modelCeiling` through `FrameDispatcher`, `ServerOptions`, `serveLocal`; `main.ts` builds the resolver from `SMOOAI_GATEWAY_URL`/`KEY` (undefined on the keyless local path ⇒ unclamped, behaviour unchanged).
- Bump `@smooai/smooth-operator-core` pin to `^0.20.4` (the published release introducing `modelMaxOutput` / `effectiveMaxTokens`).
