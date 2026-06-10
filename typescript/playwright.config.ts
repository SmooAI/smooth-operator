import { defineConfig, devices } from '@playwright/test';

/**
 * Playwright config for the chat-widget live e2e.
 *
 * Conventions (matching the smooai monorepo): chromium only, `reporter: 'list'`
 * (never the html reporter with its auto-opened blocking server). A `webServer`
 * block boots the dependency-free node static server (e2e/static-server.mjs)
 * that serves the repo root, so the demo page can load the built IIFE bundle.
 *
 * The smooth-operator-server itself is spawned from inside the spec
 * (e2e/widget.live.spec.ts) so its lifecycle is gated on the same env guards
 * (SMOOTH_AGENT_E2E + SMOOAI_GATEWAY_KEY) that skip the test.
 */
const STATIC_PORT = Number(process.env.STATIC_PORT ?? 4830);

export default defineConfig({
    testDir: './e2e',
    fullyParallel: false,
    workers: 1,
    forbidOnly: !!process.env.CI,
    retries: 0,
    reporter: 'list',
    timeout: 120_000,
    expect: { timeout: 60_000 },
    use: {
        baseURL: `http://127.0.0.1:${STATIC_PORT}`,
        trace: 'retain-on-failure',
    },
    projects: [
        {
            name: 'chromium',
            use: { ...devices['Desktop Chrome'] },
        },
    ],
    webServer: {
        command: 'node e2e/static-server.mjs',
        url: `http://127.0.0.1:${STATIC_PORT}/e2e/fixtures/demo.html`,
        reuseExistingServer: !process.env.CI,
        timeout: 30_000,
        env: { STATIC_PORT: String(STATIC_PORT) },
    },
});
