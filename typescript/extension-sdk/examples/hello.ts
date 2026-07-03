/**
 * `hello` — the Phase 1 demo extension. One tool, `hello.greet`, exercising the
 * whole tool path: a zod-typed schema, streamed progress, and cancellation.
 *
 * Run it as a real SEP subprocess:  `tsx examples/hello.ts`
 * The host handshakes, then dispatches `hello.greet` like any native tool.
 */
import { z } from 'zod';
import { defineExtension, defineTool } from '../src/index.js';

export const hello = defineExtension((smooth) => {
    smooth.name = 'hello';
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

    smooth.on('turn_start', () => smooth.log('info', 'a turn started'));
});

// When run directly (not imported by a test), serve over stdio.
if (import.meta.url === `file://${process.argv[1]}`) {
    hello.serve();
}
