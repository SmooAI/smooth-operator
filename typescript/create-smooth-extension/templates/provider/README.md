# __NAME__

An LLM-**provider** [Smooth Extension Protocol (SEP)](https://github.com/SmooAI/smooth-operator/tree/main/spec/extension) extension.

Registers a provider (`__NAME__`) the host reaches over `provider/complete`. Out
of the box it returns a canned echo response so it builds and tests green with no
network — replace the marked block in `src/index.ts` with your real completion
call (see the `corporate-proxy` demo for a full OpenAI-compatible proxy with SSE
streaming and OAuth).

## Develop

```bash
pnpm install
pnpm build      # tsc -> dist/index.js
pnpm test       # unit test (createTestHost) + SEP conformance
pnpm typecheck
```

## Configure

- `PROVIDER_BASE_URL` — the upstream OpenAI-compatible base URL.
- `PROVIDER_API_KEY` — the bearer key (or obtained via `oauthLogin`).

## Install into a host

Copy this directory to `~/.smooth/extensions/__NAME__/` (global) or
`<workspace>/.smooth/extensions/__NAME__/` (project). The host reads
`extension.toml`, launches `node dist/index.js`, and surfaces the provider's
models in its model picker.
