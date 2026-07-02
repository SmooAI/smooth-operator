#!/usr/bin/env node
// The SEP fixture-replay peer: a dependency-free demo extension used as the
// conformance target. It speaks JSON-RPC 2.0 ndjson over stdin/stdout, exactly
// as a real extension would, so any engine host can spawn it and replay
// conformance/fixtures.json against a live process instead of just schemas.
// Kept dependency-free (only node:readline/node:process) on purpose: it must
// run anywhere Node runs, with nothing to `npm install` first.

import { createInterface } from 'node:readline';
import process from 'node:process';

const rl = createInterface({ input: process.stdin, terminal: false });

function reply(id, result) {
    process.stdout.write(`${JSON.stringify({ jsonrpc: '2.0', id, result })}\n`);
}

function replyError(id, code, message) {
    process.stdout.write(`${JSON.stringify({ jsonrpc: '2.0', id, error: { code, message } })}\n`);
}

rl.on('line', (line) => {
    if (!line.trim()) return;
    const frame = JSON.parse(line);
    const { id, method, params } = frame;
    const isNotification = id === undefined;

    switch (method) {
        case 'initialize':
            reply(id, {
                protocol_version: Math.min(params?.protocol_version ?? 1, 1),
                extension: { name: 'echo', version: '0.1.0' },
                registrations: {
                    tools: [
                        {
                            name: 'say',
                            description: 'Echo a phrase back.',
                            parameters: { type: 'object', properties: { phrase: { type: 'string' } }, required: ['phrase'] },
                        },
                    ],
                    subscriptions: ['turn_start', 'turn_end', 'message_end'],
                },
            });
            break;

        case 'ping':
            reply(id, {});
            break;

        case 'hook':
            reply(id, { action: 'continue' });
            break;

        case 'tool/execute':
            reply(id, { content: params?.arguments?.phrase ?? '' });
            break;

        case 'shutdown':
            reply(id, {});
            process.exit(0);
            break;

        case 'event':
        case '$/cancel':
            // Fire-and-forget notifications this demo extension doesn't act on.
            break;

        default:
            if (!isNotification) {
                replyError(id, -32601, `method not found: ${method}`);
            }
            break;
    }
});
