// tui-chat — a terminal chat client for a running smooth-operator server.
//
// Same protocol, same published SDK (@smooai/smooth-operator) as the web-chat
// example — just a terminal front-end instead of a browser one. Dependency-free
// beyond the SDK: Node 22 ships a global WebSocket (the SDK's default transport)
// and readline drives the prompt. It demonstrates, against a real server:
//
//   • token streaming            (assistant reply grows token-by-token)
//   • inline tool-call / result  (⚙ knowledge_search → ✓/✗ result)
//   • human-in-the-loop approval (parked write tool → y/N → SDK resumes the turn)
//   • durable conversations      (/list, /resume <id>, /new)
//
// Commands:  /new  ·  /list  ·  /resume <conversationId>  ·  /exit
import { SmoothAgentClient } from '@smooai/smooth-operator';
import readline from 'node:readline';

const URL = process.env.SMOOTH_WS_URL ?? 'ws://localhost:8787/ws';
const TOKEN = process.env.SMOOTH_TOKEN || undefined;
const AGENT_ID = process.env.SMOOTH_AGENT_ID ?? crypto.randomUUID();

// ── tiny ANSI palette (no chalk) ─────────────────────────────────────────────
const paint = (code, s) => `\x1b[${code}m${s}\x1b[0m`;
const c = {
    dim: (s) => paint('2', s),
    bold: (s) => paint('1', s),
    cyan: (s) => paint('36', s),
    green: (s) => paint('32', s),
    red: (s) => paint('31', s),
    yellow: (s) => paint('33', s),
    magenta: (s) => paint('35', s),
};

const rl = readline.createInterface({ input: process.stdin, output: process.stdout });
const ask = (q) => new Promise((res) => rl.question(q, res));

/** Connect, retrying briefly so `docker compose run` doesn't lose a race with a
 * just-started operator container. */
async function connectWithRetry(client, attempts = 20) {
    for (let i = 1; i <= attempts; i++) {
        try {
            await client.connect();
            return;
        } catch (err) {
            if (i === attempts) throw err;
            process.stdout.write(c.dim(`\r… waiting for ${URL} (${i}/${attempts})`));
            await new Promise((r) => setTimeout(r, 1000));
        }
    }
}

async function main() {
    console.log(c.bold(c.magenta('\n  smooth-operator · terminal chat')));
    console.log(c.dim(`  server ${URL}`));

    const client = new SmoothAgentClient({ url: URL, token: TOKEN, turnTimeout: 60_000 });
    await connectWithRetry(client);

    let session = await client.createConversationSession({ agentId: AGENT_ID, userName: 'tui-chat-example' });
    console.log(c.green(`  connected`) + c.dim(`  ·  conversation ${session.conversationId}`));
    console.log(c.dim('  commands: /new · /list · /resume <id> · /exit') + '\n');

    // Consume one streaming turn, rendering text + tool chips inline and pausing
    // for a human verdict when the server parks a write tool.
    async function runTurn(text) {
        const turn = client.sendMessage({ sessionId: session.sessionId, message: text, stream: true });
        process.stdout.write(c.cyan('  agent  '));
        let atLineStart = false;
        try {
            for await (const ev of turn) {
                const v = ev;
                switch (v.type) {
                    case 'stream_token': {
                        process.stdout.write(v.token ?? v.data?.token ?? '');
                        atLineStart = false;
                        break;
                    }
                    case 'stream_chunk': {
                        const st = v.data?.state?.rawResponse;
                        if (st?.toolCall) {
                            const call = st.toolCall;
                            const args = typeof call.arguments === 'string' ? call.arguments : JSON.stringify(call.arguments ?? {});
                            process.stdout.write(`\n  ${c.yellow('⚙ ' + (call.name ?? 'tool'))} ${c.dim(args)}`);
                            atLineStart = false;
                        } else if (st?.toolResult) {
                            const res = st.toolResult;
                            const body = typeof res.result === 'string' ? res.result : JSON.stringify(res.result ?? '');
                            const mark = res.isError ? c.red('✗') : c.green('✓');
                            process.stdout.write(`\n  ${mark} ${c.dim(body.slice(0, 200))}\n  ${c.cyan('agent  ')}`);
                            atLineStart = false;
                        }
                        break;
                    }
                    case 'write_confirmation_required': {
                        const d = v.data?.data ?? v.data ?? {};
                        process.stdout.write('\n');
                        const answer = (await ask(c.yellow(`  ⚠ approve ${d.toolId ?? 'tool'}? `) + c.dim(`${d.actionDescription ?? ''} [y/N] `))).trim().toLowerCase();
                        const approved = answer === 'y' || answer === 'yes';
                        client.confirmToolAction({ sessionId: session.sessionId, requestId: turn.requestId, approved });
                        process.stdout.write(`  ${approved ? c.green('approved') : c.red('denied')}\n  ${c.cyan('agent  ')}`);
                        atLineStart = false;
                        break;
                    }
                    default:
                        break;
                }
            }
            await turn;
        } catch (err) {
            process.stdout.write(`\n  ${c.red('error')} ${c.dim(err instanceof Error ? err.message : String(err))}`);
        }
        if (!atLineStart) process.stdout.write('\n\n');
    }

    // Main REPL.
    for (;;) {
        const line = (await ask(c.bold('  you  '))).trim();
        if (!line) continue;

        if (line === '/exit' || line === '/quit') break;
        if (line === '/new') {
            session = await client.createConversationSession({ agentId: AGENT_ID, userName: 'tui-chat-example' });
            console.log(c.dim(`  new conversation ${session.conversationId}\n`));
            continue;
        }
        if (line === '/list') {
            const { conversations } = await client.listConversations();
            if (!conversations.length) console.log(c.dim('  (no conversations yet)\n'));
            for (const conv of conversations) {
                console.log(`  ${c.cyan(conv.conversationId)} ${c.dim(conv.title ?? conv.lastMessagePreview ?? '')}`);
            }
            console.log('');
            continue;
        }
        if (line.startsWith('/resume ')) {
            const id = line.slice('/resume '.length).trim();
            session = await client.createConversationSession({ agentId: AGENT_ID, conversationId: id, userName: 'tui-chat-example' });
            const { messages } = await client.getMessages({ sessionId: session.sessionId });
            const chronological = messages.slice().sort((a, b) => (Date.parse(a?.createdAt ?? '') || 0) - (Date.parse(b?.createdAt ?? '') || 0));
            for (const m of chronological) {
                const isUser = m?.direction === 'inbound' || m?.role === 'user';
                const body = m?.content?.text ?? (typeof m?.content === 'string' ? m.content : '') ?? '';
                console.log(`  ${isUser ? c.bold('you  ') : c.cyan('agent')}  ${body}`);
            }
            console.log(c.dim(`  resumed ${id}\n`));
            continue;
        }

        await runTurn(line);
    }

    client.disconnect('tui exit');
    rl.close();
    console.log(c.dim('  bye.\n'));
    process.exit(0);
}

main().catch((err) => {
    console.error(c.red(`\n  fatal: ${err instanceof Error ? err.message : String(err)}`));
    process.exit(1);
});
