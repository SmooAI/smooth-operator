# __NAME__

A [Smooth Extension Protocol (SEP)](https://github.com/SmooAI/smooth-operator/tree/main/spec/extension) extension that contributes a **tool**.

`greet` demonstrates the full tool path: a zod-typed schema, streamed progress, and cancellation.

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
