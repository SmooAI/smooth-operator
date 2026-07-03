# __NAME__

A minimal, **provider-less** [Smooth Extension Protocol (SEP)](https://github.com/SmooAI/smooth-operator/tree/main/spec/extension) extension.

One tool (`shout`) doing pure local computation — no AI provider, network call,
API key, or secret. The simplest useful starting point.

## Develop

```bash
pnpm install
pnpm build      # tsc -> dist/index.js
pnpm test       # unit test (createTestHost) + SEP conformance
pnpm typecheck
```

## Install into a host

Copy this directory to `~/.smooth/extensions/__NAME__/` (global) or
`<workspace>/.smooth/extensions/__NAME__/` (project). The host reads
`extension.toml` and launches `node dist/index.js`.

Edit `src/index.ts` to add your own tools.
