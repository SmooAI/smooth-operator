/**
 * `runConformance` replays the shared SEP fixtures against a REAL extension
 * subprocess. Target: the canonical dependency-free `echo.mjs` peer that ships
 * with the spec (it registers `say`, exactly what the tool_execute fixture
 * calls). This is the SDK's live-wire gate — distinct from the schema-only
 * fixture validation in the spec repo.
 */
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { describe, expect, it } from 'vitest';
import { runConformance } from '../src/index.js';

const __dirname = dirname(fileURLToPath(import.meta.url));
const ECHO_PEER = join(__dirname, '..', '..', '..', 'spec', 'extension', 'conformance', 'echo.mjs');

describe('runConformance against the echo.mjs subprocess', () => {
    it('handshakes and answers every method with a schema-valid reply', async () => {
        const report = await runConformance({ command: process.execPath, args: [ECHO_PEER] });
        const failed = report.steps.filter((s) => !s.ok);
        expect(failed, JSON.stringify(failed)).toHaveLength(0);
        expect(report.passed).toBe(true);
        expect(report.steps.map((s) => s.name)).toEqual(['initialize', 'ping', 'tool/execute', 'shutdown']);
    });
});
