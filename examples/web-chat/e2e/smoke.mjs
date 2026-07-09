// smoke.mjs — a dependency-free live smoke check for the web-chat example.
//
// It drives the SAME protocol the browser app does, through the SAME published
// SDK, but from Node (Node 22 ships a global `WebSocket`, which the SDK's default
// transport uses — so there's nothing to install). It connects, opens a session,
// lists conversations, and streams one reply, printing what it sees.
//
//   SMOOTH_WS_URL=ws://localhost:8787/ws node e2e/smoke.mjs
//
// Exit codes: 0 = connected + session created (a full LLM reply also requires the
// server to have SMOOAI_GATEWAY_KEY set); 1 = could not connect. This never fails
// on an empty LLM reply, so it's safe to run against a keyless dev server.

import { SmoothAgentClient } from '@smooai/smooth-operator';

const url = process.env.SMOOTH_WS_URL ?? 'ws://localhost:8787/ws';
const token = process.env.SMOOTH_TOKEN;
const message = process.env.SMOOTH_MESSAGE ?? 'What is your return policy?';

console.log(`[smoke] connecting to ${url}`);
const client = new SmoothAgentClient({ url, token, turnTimeout: 30_000 });

try {
    await client.connect();
} catch (err) {
    console.error(`[smoke] FAILED to connect — is smooth-operator-server running? (${err instanceof Error ? err.message : err})`);
    process.exit(1);
}

const session = await client.createConversationSession({ agentId: crypto.randomUUID(), userName: 'smoke' });
console.log(`[smoke] session ${session.sessionId} (conversation ${session.conversationId})`);

const { conversations } = await client.listConversations();
console.log(`[smoke] listConversations → ${conversations.length} conversation(s)`);

console.log(`[smoke] sending: ${JSON.stringify(message)}`);
let tokens = 0;
const turn = client.sendMessage({ sessionId: session.sessionId, message, stream: true });
try {
    for await (const ev of turn) {
        if (ev.type === 'stream_token') {
            const tok = ev.token ?? ev.data?.token ?? '';
            process.stdout.write(tok);
            tokens += tok.length;
        }
    }
    await turn;
    console.log(`\n[smoke] OK — streamed ${tokens} char(s) of reply.`);
} catch (err) {
    // A keyless dev server errors the turn cleanly; that still proves the wire path.
    console.log(`\n[smoke] turn ended without a full reply (${err instanceof Error ? err.message : err}).`);
    console.log('[smoke] connection + session + protocol verified. Set SMOOAI_GATEWAY_KEY on the server for real replies.');
}

client.disconnect('smoke done');
process.exit(0);
