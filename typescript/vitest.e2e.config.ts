import { defineConfig } from 'vitest/config';

/**
 * Dedicated config for the live-gateway E2E. The default `vitest.config.ts`
 * deliberately *excludes* `test/e2e.live.test.ts` so `pnpm test` stays
 * credential-free and green in CI; this config targets only that file so
 * `pnpm test:e2e` can run it. The test itself still self-skips unless
 * `SMOOTH_AGENT_E2E=1` and `SMOOAI_GATEWAY_KEY` are set.
 */
export default defineConfig({
    test: {
        include: ['test/e2e.live.test.ts'],
        environment: 'node',
        reporters: ['default'],
        // Long real-LLM turns; give the suite headroom over the per-test timeout.
        testTimeout: 400_000,
        hookTimeout: 60_000,
    },
});
