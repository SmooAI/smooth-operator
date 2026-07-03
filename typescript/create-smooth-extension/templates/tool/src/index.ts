/**
 * __NAME__ — a SEP extension that contributes a tool.
 *
 * The tool exercises the whole tool path: a zod-typed schema, streamed progress
 * (`ctx.onUpdate`), and cooperative cancellation (`ctx.signal`). Run it as a
 * real SEP subprocess with `node dist/index.js` (the host handshakes over stdio
 * and dispatches `greet` like any native tool).
 */
import { z } from 'zod';
import { defineExtension, defineTool } from '@smooai/smooth-extension-sdk';

export const extension = defineExtension((smooth) => {
    smooth.name = '__NAME__';
    smooth.version = '0.1.0';

    smooth.registerTool(
        defineTool({
            name: 'greet',
            description: 'Greet someone by name.',
            parameters: z.object({ name: z.string().describe('Who to greet.') }),
            async execute(args, ctx) {
                ctx.onUpdate({ message: `greeting ${args.name}`, progress: 0.5 });
                return { content: `Hello, ${args.name}!` };
            },
        }),
    );
});

// When run directly (not imported by a test), serve over stdio.
if (import.meta.url === `file://${process.argv[1]}`) {
    extension.serve();
}
