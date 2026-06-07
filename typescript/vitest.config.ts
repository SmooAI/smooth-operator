import { defineConfig } from 'vitest/config';

export default defineConfig({
    test: {
        include: ['test/**/*.test.ts', 'test/**/*.test-d.ts'],
        // The live-gateway E2E (test/e2e.live.test.ts) is excluded from the default
        // `vitest run` so `pnpm test` stays credential-free and green in CI. Run it
        // explicitly via `pnpm test:e2e`, which uses `vitest.e2e.config.ts`.
        exclude: ['node_modules/**', 'dist/**', 'test/e2e.live.test.ts'],
        environment: 'node',
        reporters: ['default'],
    },
});
