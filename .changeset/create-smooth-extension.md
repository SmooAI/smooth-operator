---
'create-smooth-extension': minor
---

Add `create-smooth-extension` — the `npm create smooth-extension` scaffolder for SEP (Smooth Extension Protocol) extensions.

Ships four templates, each a complete project that typechecks, builds with `tsc`, unit-tests with `createTestHost`, and passes `runConformance` against the vendored `spec/extension` fixtures:

- **tool** — a tool with a zod schema, streamed progress, and cancellation.
- **permission-gate** — a fail-closed `tool_call` hook that vetoes dangerous bash commands.
- **command** — a slash-command with argument autocomplete.
- **provider-less** — a minimal, self-contained tool needing no AI provider, network, or secret.

Each scaffold includes an `extension.toml` with the right `[capabilities]` for its kind, a `src/index.ts` using `@smooai/smooth-extension-sdk` and `serve()`, a unit test, a conformance test, tsconfig, vitest config, `.gitignore`, and a README.
