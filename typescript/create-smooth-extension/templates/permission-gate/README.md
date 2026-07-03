# __NAME__

A [Smooth Extension Protocol (SEP)](https://github.com/SmooAI/smooth-operator/tree/main/spec/extension) extension that **gates tool calls**.

It intercepts the fail-closed `tool_call` hook and blocks dangerous `bash`
commands before they run — the model can still read and think, but destructive
commands are vetoed.

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

Edit the `DANGEROUS` patterns in `src/index.ts` to fit your policy.
