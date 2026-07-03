/**
 * Unit tests for the scaffolder: arg parsing plus a real generation into a tmp
 * dir, asserting the file set, `__NAME__` substitution, and the renames
 * (`_package.json` -> package.json, `_gitignore` -> .gitignore). The full
 * install/build/conformance proof lives outside this fast unit test.
 */
import { existsSync, mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, describe, expect, it } from 'vitest';
import { parseArgs, scaffold } from '../src/index.js';

let dir: string | undefined;
afterEach(() => {
    if (dir) rmSync(dir, { recursive: true, force: true });
    dir = undefined;
});

describe('parseArgs', () => {
    it('reads the name positional and --template flag', () => {
        expect(parseArgs(['my-ext', '--template', 'command'])).toEqual({ name: 'my-ext', template: 'command' });
    });
    it('supports -t and --template=', () => {
        expect(parseArgs(['-t', 'tool', 'x'])).toEqual({ name: 'x', template: 'tool' });
        expect(parseArgs(['x', '--template=provider-less'])).toEqual({ name: 'x', template: 'provider-less' });
    });
});

describe('scaffold', () => {
    for (const template of ['tool', 'permission-gate', 'command', 'provider-less', 'provider'] as const) {
        it(`generates a complete ${template} project`, () => {
            dir = mkdtempSync(join(tmpdir(), 'cse-'));
            const target = join(dir, 'proj');
            scaffold(template, 'demo-ext', target);

            // Shared + template files landed, with the scaffold-safe renames applied.
            for (const f of ['package.json', '.gitignore', 'tsconfig.json', 'vitest.config.ts', 'extension.toml', 'src/index.ts', 'test/conformance.test.ts']) {
                expect(existsSync(join(target, f)), f).toBe(true);
            }
            expect(existsSync(join(target, '_package.json'))).toBe(false);
            expect(existsSync(join(target, 'spec/extension/conformance/fixtures.json'))).toBe(true);

            // __NAME__ substituted everywhere it appears.
            const pkg = JSON.parse(readFileSync(join(target, 'package.json'), 'utf8'));
            expect(pkg.name).toBe('demo-ext');
            expect(pkg.dependencies['@smooai/smooth-extension-sdk']).toBeTruthy();
            expect(readFileSync(join(target, 'extension.toml'), 'utf8')).toContain('name = "demo-ext"');
            expect(readFileSync(join(target, 'src/index.ts'), 'utf8')).toContain("smooth.name = 'demo-ext'");
            // No placeholder leaks through.
            expect(readFileSync(join(target, 'src/index.ts'), 'utf8')).not.toContain('__NAME__');
        });
    }
});
