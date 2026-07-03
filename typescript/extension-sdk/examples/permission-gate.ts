/**
 * `permission-gate` — the Phase 2 demo extension. It vetoes dangerous `bash`
 * commands via the fail-closed `tool_call` hook before the tool ever runs.
 *
 * Run it as a real SEP subprocess:  `tsx examples/permission-gate.ts`
 * The host runs this hook before executing any tool; a `block` stops the call.
 * Because `tool_call` is fail-closed, if this process hangs or crashes the host
 * times out and blocks the tool anyway — safe by default.
 */
import { defineExtension } from '../src/index.js';

/** Commands that should never run unattended. Add patterns as needed. */
const DANGEROUS: RegExp[] = [
    /\brm\s+-[a-z]*[rf]/, // rm -rf / rm -fr / rm -r ...
    /\bmkfs\b/, // format a filesystem
    /\bdd\s+.*\bof=\/dev\//, // dd onto a raw device
    />\s*\/dev\/sd[a-z]/, // redirect onto a raw disk
    /:\(\)\s*\{.*:\|:.*\}\s*;\s*:/, // classic fork bomb
    /\bchmod\s+-R\s+0*777\s+\//, // chmod -R 777 /
];

export const permissionGate = defineExtension((smooth) => {
    smooth.name = 'permission-gate';
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
    permissionGate.serve();
}
