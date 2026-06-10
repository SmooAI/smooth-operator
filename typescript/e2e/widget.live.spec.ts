/**
 * Live e2e (feature gap G5): load the built `<smooth-agent-chat>` widget in a real
 * browser, point it at a locally-spawned `smooth-operator-server`, send a
 * message, and assert the streamed, knowledge-grounded reply renders.
 *
 * The server seeds a distinctive KB doc on startup (SMOOTH_AGENT_SEED_KB=1):
 *   "SmooAI's return window is exactly 17 days from delivery."
 * so a grounded answer to "What is SmooAI's return window?" must contain "17".
 *
 * Gating: this hits the live llm.smoo.ai gateway and costs money, so it only
 * runs when BOTH guards are set:
 *   - SMOOTH_AGENT_E2E=1
 *   - SMOOAI_GATEWAY_KEY=<key>   (read from env; never hardcoded/printed)
 * Otherwise it skips cleanly. The key is passed into the spawned server's env
 * only — it is never logged.
 */
import { type ChildProcess, spawn } from 'node:child_process';
import { existsSync } from 'node:fs';
import { createConnection } from 'node:net';
import { homedir } from 'node:os';
import { join } from 'node:path';
import { expect, test } from '@playwright/test';

const AGENT_PORT = 8830;
const SERVER_BIN = join(homedir(), '.cargo', 'shared-target', 'debug', 'smooth-operator-server');

const GATEWAY_KEY = process.env.SMOOAI_GATEWAY_KEY ?? '';
const E2E_ENABLED = process.env.SMOOTH_AGENT_E2E === '1' && GATEWAY_KEY.length > 0;

/** Resolve once a TCP connect to host:port succeeds, polling up to `timeoutMs`. */
async function waitForPort(host: string, port: number, timeoutMs: number): Promise<void> {
    const deadline = Date.now() + timeoutMs;
    for (;;) {
        const ok = await new Promise<boolean>((resolve) => {
            const sock = createConnection({ host, port }, () => {
                sock.destroy();
                resolve(true);
            });
            sock.on('error', () => {
                sock.destroy();
                resolve(false);
            });
        });
        if (ok) return;
        if (Date.now() > deadline) throw new Error(`port ${host}:${port} not ready within ${timeoutMs}ms`);
        await new Promise((r) => setTimeout(r, 250));
    }
}

let server: ChildProcess | null = null;

test.beforeAll(async () => {
    test.skip(!E2E_ENABLED, 'Set SMOOTH_AGENT_E2E=1 and SMOOAI_GATEWAY_KEY to run the live widget e2e.');

    if (!existsSync(SERVER_BIN)) {
        throw new Error(
            `smooth-operator-server binary not found at ${SERVER_BIN}. ` +
                'Build it with: cargo build -p smooai-smooth-operator-server --bin smooth-operator-server',
        );
    }

    // Spawn the server with the gateway key + KB seed. The key only enters the
    // child env here — it is never written to logs or stdout by this test.
    server = spawn(SERVER_BIN, [], {
        env: {
            ...process.env,
            SMOOTH_AGENT_BIND: '127.0.0.1',
            SMOOTH_AGENT_PORT: String(AGENT_PORT),
            SMOOAI_GATEWAY_KEY: GATEWAY_KEY,
            SMOOTH_AGENT_SEED_KB: '1',
            SMOOTH_AGENT_MODEL: process.env.SMOOTH_AGENT_MODEL ?? 'claude-haiku-4-5',
        },
        stdio: ['ignore', 'pipe', 'pipe'],
    });

    // Surface server stdout/stderr for debugging, but never echo the env.
    server.stdout?.on('data', (d: Buffer) => process.stdout.write(`[agent-server] ${d}`));
    server.stderr?.on('data', (d: Buffer) => process.stderr.write(`[agent-server] ${d}`));
    server.on('exit', (code, signal) => {
        if (code !== 0 && code !== null) {
            process.stderr.write(`[agent-server] exited code=${code} signal=${signal}\n`);
        }
    });

    await waitForPort('127.0.0.1', AGENT_PORT, 20_000);
});

test.afterAll(async () => {
    if (server && !server.killed) {
        server.kill('SIGTERM');
        // Give it a moment, then force-kill if still alive.
        await new Promise((r) => setTimeout(r, 500));
        if (!server.killed) server.kill('SIGKILL');
    }
    server = null;
});

test('widget renders streamed, knowledge-grounded reply containing "17"', async ({ page }) => {
    test.skip(!E2E_ENABLED, 'Set SMOOTH_AGENT_E2E=1 and SMOOAI_GATEWAY_KEY to run the live widget e2e.');

    // Capture browser console (surfaces WebSocket / protocol errors on failure).
    const consoleLines: string[] = [];
    page.on('console', (msg) => consoleLines.push(`[console:${msg.type()}] ${msg.text()}`));
    page.on('pageerror', (err) => consoleLines.push(`[pageerror] ${err.message}`));

    await page.goto('/e2e/fixtures/demo.html');
    await page.waitForLoadState('load');

    const widget = page.locator('smooth-agent-chat');
    await expect(widget).toBeAttached();

    // The widget renders into a shadow root. `start-open` means the panel is
    // already open; the textarea + Send button live inside the shadow DOM.
    // Playwright pierces shadow roots automatically with CSS locators.
    const input = widget.locator('textarea');
    const sendBtn = widget.locator('button.send');

    // Wait for the composer to be enabled (it disables while `connecting`).
    await expect(input).toBeVisible();
    await expect(sendBtn).toBeEnabled();

    await input.fill("What is SmooAI's return window? Search the knowledge base.");
    await sendBtn.click();

    // The user bubble appears immediately; the assistant bubble grows as tokens
    // stream in. Poll the latest assistant bubble until it contains "17".
    const assistantBubble = widget.locator('.bubble.assistant').last();

    try {
        await expect
            .poll(async () => (await assistantBubble.textContent()) ?? '', {
                message: 'assistant bubble should stream a grounded reply containing "17"',
                timeout: 90_000,
            })
            .toContain('17');
    } catch (err) {
        // Dump captured browser console to make WS/protocol failures debuggable.
        console.log('\n--- browser console ---\n' + consoleLines.join('\n') + '\n--- end console ---\n');
        throw err;
    }

    const rendered = (await assistantBubble.textContent())?.trim() ?? '';
    expect(rendered).toContain('17');
    console.log(`\n[e2e] rendered assistant reply:\n${rendered}\n`);
});
