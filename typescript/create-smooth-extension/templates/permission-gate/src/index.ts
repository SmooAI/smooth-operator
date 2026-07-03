/**
 * __NAME__ — a SEP extension that gates tool calls.
 *
 * It intercepts the fail-closed `tool_call` hook and vetoes dangerous `bash`
 * commands before the tool ever runs. Because `tool_call` is fail-closed, if
 * this process hangs or crashes the host times out and blocks the call anyway —
 * safe by default. Run it with `node dist/index.js`.
 */
import { defineExtension } from '@smooai/smooth-extension-sdk';

/** Commands that should never run unattended. Add patterns as needed. */
const DANGEROUS: RegExp[] = [
    /\brm\s+-[a-z]*[rf]/, // rm -rf / rm -fr / rm -r ...
    /\bmkfs\b/, // format a filesystem
    /\bdd\s+.*\bof=\/dev\//, // dd onto a raw device
    /:\(\)\s*\{.*:\|:.*\}\s*;\s*:/, // classic fork bomb
];

export const extension = defineExtension((smooth) => {
    smooth.name = '__NAME__';
    smooth.version = '0.1.0';

    smooth.on('tool_call', (input) => {
        if (input?.tool !== 'bash') return;
        const command = String((input.arguments as Record<string, unknown> | undefined)?.command ?? '');
        const hit = DANGEROUS.find((re) => re.test(command));
        if (hit) return { block: true, reason: `blocked dangerous command (matched ${hit})` };
    });
});

// When run directly (not imported by a test), serve over stdio.
if (import.meta.url === `file://${process.argv[1]}`) {
    extension.serve();
}
