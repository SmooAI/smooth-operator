import { defineConfig } from 'vitest/config';

export default defineConfig({
    test: {
        include: ['test/**/*.test.ts'],
        environment: 'node',
        // The graceful-drain and turn round-trip tests stand up a real ws server +
        // client over loopback; a slightly generous timeout keeps them stable in CI.
        testTimeout: 15_000,
    },
});
