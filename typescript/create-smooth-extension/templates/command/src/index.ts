/**
 * __NAME__ — a SEP extension that contributes a slash-command.
 *
 * `/echo` surfaces text into the session and demonstrates argument
 * autocomplete (`complete`). Commands run at the command tier, so they may also
 * drive session actions (`ctx.session`) and UI (`ctx.ui`). Run it with
 * `node dist/index.js`.
 */
import { defineCommand, defineExtension } from '@smooai/smooth-extension-sdk';

const SUGGESTIONS = ['hello', 'ready', 'done'];

export const extension = defineExtension((smooth) => {
    smooth.name = '__NAME__';
    smooth.version = '0.1.0';

    smooth.registerCommand(
        defineCommand({
            name: 'echo',
            description: 'Echo a phrase back into the session.',
            execute(ctx) {
                const phrase = String(ctx.args?.phrase ?? '').trim();
                return { content: phrase ? `You said: ${phrase}` : 'Usage: /echo <phrase>' };
            },
            complete: (partial) => SUGGESTIONS.filter((v) => v.startsWith(partial)).map((value) => ({ value })),
        }),
    );
});

// When run directly (not imported by a test), serve over stdio.
if (import.meta.url === `file://${process.argv[1]}`) {
    extension.serve();
}
