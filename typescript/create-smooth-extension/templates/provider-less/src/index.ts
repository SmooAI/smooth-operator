/**
 * __NAME__ — a minimal, provider-less SEP extension.
 *
 * One tool doing pure local computation: no AI provider, no network call, no
 * API key or secret. The simplest possible useful extension — the starting
 * point when your tool just transforms its inputs. Run it with
 * `node dist/index.js`.
 */
import { z } from 'zod';
import { defineExtension, defineTool } from '@smooai/smooth-extension-sdk';

export const extension = defineExtension((smooth) => {
    smooth.name = '__NAME__';
    smooth.version = '0.1.0';

    smooth.registerTool(
        defineTool({
            name: 'shout',
            description: 'Uppercase a string. Pure local computation — no provider needed.',
            parameters: z.object({ text: z.string().describe('The text to uppercase.') }),
            execute(args) {
                return { content: args.text.toUpperCase() };
            },
        }),
    );
});

// When run directly (not imported by a test), serve over stdio.
if (import.meta.url === `file://${process.argv[1]}`) {
    extension.serve();
}
