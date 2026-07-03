# @smooai/smooth-extension-sdk

Build **SEP** (Smooth Extension Protocol) extensions in TypeScript.

An extension is a long-lived subprocess that speaks JSON-RPC 2.0 over ndjson on
its stdin/stdout to a SEP host (`smooth-operator-core` and its polyglot servers).
This SDK is the DX centerpiece: describe your extension declaratively, `serve()`
it, test it in-process, and gate it against the shared conformance fixtures.

## Quick start

```ts
import { z } from 'zod';
import { defineExtension, defineTool } from '@smooai/smooth-extension-sdk';

export const hello = defineExtension((smooth) => {
    smooth.name = 'hello';
    smooth.version = '0.1.0';

    smooth.registerTool(
        defineTool({
            name: 'greet',
            description: 'Greet someone by name.',
            parameters: z.object({ name: z.string() }),
            async execute(args, ctx) {
                ctx.onUpdate({ message: `greeting ${args.name}`, progress: 0.5 });
                return { content: `Hello, ${args.name}!` };
            },
        }),
    );
});

hello.serve(); // wire to stdin/stdout and run
```

The host exposes the tool to the LLM as `hello.greet`.

## Schemas

`parameters` accepts three shapes — the wire truth is always JSON Schema:

- a **zod v4** schema → converted with `z.toJSONSchema()`
- a **TypeBox** schema → TypeBox schemas already ARE JSON Schema, passed through
- a **raw JSON Schema** object → passed through unchanged

## Tool context

`execute(args, ctx)` receives a `ctx` with:

- `ctx.onUpdate({ message?, progress?, details? })` — stream `tool/update` progress
- `ctx.signal` — an `AbortSignal` that fires when the host sends `$/cancel`
- `ctx.callId` / `ctx.context` — the call id and dispatch context (epoch token + tier)

Return a `{ content, is_error?, details? }` result, or just a string shorthand for
`{ content }`.

## Testing

```ts
import { createTestHost } from '@smooai/smooth-extension-sdk';
import { hello } from './hello.js';

const host = createTestHost(hello); // in-process, no subprocess
await host.initialize();
const res = await host.callTool('greet', { name: 'Ada' });
// res === { content: 'Hello, Ada!' }
host.close();
```

`runConformance` replays the shared SEP fixtures against a **real** extension
subprocess, validating every reply against its schema:

```ts
import { runConformance } from '@smooai/smooth-extension-sdk';

const report = await runConformance({ command: 'node', args: ['./hello.js'] });
// report.passed === true
```

## API

- `defineExtension((smooth) => void)` — set `smooth.name`/`version`, `registerTool`, `on(event)`, `log`.
- `defineTool({ name, description, parameters, deferred?, execute })`
- `createTestHost(extension)` → `{ initialize, callTool, ping, sendEvent, shutdown, close }`
- `runConformance({ command, args?, env?, cwd?, specDir? })` → `ConformanceReport`
- `Peer`, `stdioTransport`, `linkedPair`, `toJsonSchema` — the building blocks
- `PROTOCOL_VERSION`, `method`, `errorCode` and the wire types

## Scope (Phase 1)

The tool path: registration, execute, streamed progress, cancellation, plus
observe `on(event)` subscriptions and lifecycle (`initialize`/`ping`/`shutdown`).
Hooks, commands, ui/kv/session/exec land in later phases — the wire and API were
shaped to grow into them without breaking the tool path.
