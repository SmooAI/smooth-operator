/**
 * SEP conformance: spawn the built extension as a real subprocess and replay
 * the shared protocol fixtures against it, validating every reply against its
 * schema. This is the same gate the SDK runs against its own examples.
 *
 * The fixtures + schemas are vendored under `spec/extension/` (a snapshot of
 * SEP v1). `pnpm test` builds `dist/index.js` first, which this spawns.
 */
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { describe, expect, it } from 'vitest';
import { runConformance } from '@smooai/smooth-extension-sdk';

const __dirname = dirname(fileURLToPath(import.meta.url));
const SPEC_DIR = join(__dirname, '..', 'spec', 'extension');
const ENTRY = join(__dirname, '..', 'dist', 'index.js');

describe('SEP conformance', () => {
    it('handshakes and answers every method with a schema-valid reply', async () => {
        const report = await runConformance({ command: process.execPath, args: [ENTRY], specDir: SPEC_DIR });
        const failed = report.steps.filter((s) => !s.ok);
        expect(failed, JSON.stringify(failed, null, 2)).toHaveLength(0);
        expect(report.passed).toBe(true);
    });
});
