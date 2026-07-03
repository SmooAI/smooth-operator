# __NAME__

A [Smooth Extension Protocol (SEP)](https://github.com/SmooAI/smooth-operator/tree/main/spec/extension) extension that contributes a **slash-command**.

`/echo` surfaces text into the session and demonstrates argument autocomplete.
Commands run at the command tier, so they can also drive session actions
(`ctx.session`) and UI (`ctx.ui`).

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

Edit `src/index.ts` to add your own commands.
