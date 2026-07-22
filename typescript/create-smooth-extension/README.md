# @smooai/create-smooth-extension

Scaffold a new [SEP (Smooth Extension Protocol)](https://github.com/SmooAI/smooth-operator/tree/main/spec/extension) extension.

```bash
npm create @smooai/smooth-extension@latest my-extension -- --template tool
# or: pnpm create @smooai/smooth-extension my-extension --template tool
# or: yarn create @smooai/smooth-extension my-extension --template tool
```

Run with no arguments to be prompted for the name and template.

## Templates

| Template         | Demonstrates                                                        |
| ---------------- | ------------------------------------------------------------------ |
| `tool`           | A tool with a zod schema, streamed progress, and cancellation.     |
| `permission-gate`| A fail-closed `tool_call` hook that vetoes dangerous bash commands.|
| `command`        | A slash-command with argument autocomplete.                        |
| `provider-less`  | A minimal, self-contained tool — no AI provider, network, or key.  |

## What you get

Every scaffolded project is complete and runnable:

- `extension.toml` — manifest with the right `[capabilities]` for the kind.
- `src/index.ts` — uses `@smooai/smooth-extension-sdk` and `serve()`s over stdio.
- A unit test driving the extension in-process with `createTestHost`.
- A SEP conformance test spawning the built extension as a real subprocess and
  replaying the shared protocol fixtures (`spec/extension/`) against it.
- `tsconfig.json` (build via `tsc`), `vitest.config.ts`, `.gitignore`, `README.md`.

```bash
cd my-extension
pnpm install
pnpm build       # tsc -> dist/index.js
pnpm test        # unit test + SEP conformance
pnpm typecheck
```
