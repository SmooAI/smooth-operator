/**
 * Conformance: every instance in `spec/extension/conformance/fixtures.json` must
 * validate against the SEP method schema it claims to (mirrors conformance.test.ts,
 * but targets spec/extension/methods/ instead of spec/{actions,events}/ and builds
 * its own ajv instance rather than importing src/validate.ts, since SEP methods
 * aren't wired into that module's ActionType/EventType discriminators).
 */
import { readFile, readdir } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import { beforeAll, describe, expect, it } from 'vitest';
import _Ajv2020, { Ajv2020 as AjvClass, type ValidateFunction, type ErrorObject as AjvError } from 'ajv/dist/2020.js';
import _addFormats from 'ajv-formats';

// ajv (and ajv-formats) ship as CJS with a double-default under NodeNext, so the
// runtime constructor can be nested one level deeper than the imported binding.
// Mirrors the exact normalization in src/validate.ts.
type Ajv = AjvClass;
const Ajv2020 = ((_Ajv2020 as unknown as { default?: unknown }).default ?? _Ajv2020) as typeof AjvClass;
const addFormats = ((_addFormats as unknown as { default?: unknown }).default ?? _addFormats) as (ajv: Ajv) => Ajv;

const __dirname = dirname(fileURLToPath(import.meta.url));
const SPEC_EXTENSION_DIR = join(__dirname, '..', '..', 'spec', 'extension');

interface Fixture {
    $schema_ref: string;
    description: string;
    instance: unknown;
}

interface InvalidFixture {
    name: string;
    $schema_ref: string;
    instance: unknown;
}

let ajv: Ajv;
/** method schema filename (e.g. "initialize.schema.json") → its $id */
const fileToId = new Map<string, string>();
const compileCache = new Map<string, ValidateFunction>();

let fixtures: Record<string, Fixture>;
let invalidFixtures: InvalidFixture[];

/** Validate `instance` against a fixtures.json-style ref: `methods/<file>.schema.json#/$defs/...`. */
function validateAt(schemaRef: string, instance: unknown): { valid: boolean; errors: AjvError[] } {
    let validate = compileCache.get(schemaRef);
    if (!validate) {
        const [path, pointer] = schemaRef.split('#');
        const file = path!.split('/').pop()!;
        const id = fileToId.get(file);
        if (!id) throw new Error(`No schema registered for "${file}" (ref "${schemaRef}")`);
        const uri = pointer ? `${id}#${pointer}` : id;
        const resolved = ajv.getSchema(uri);
        if (!resolved) throw new Error(`ajv could not resolve schema ref "${schemaRef}" (uri "${uri}")`);
        validate = resolved;
        compileCache.set(schemaRef, validate);
    }
    const valid = validate(instance) as boolean;
    return { valid, errors: valid ? [] : (validate.errors ?? []) };
}

function formatErrors(errors: AjvError[]): string {
    return errors.map((e) => `${e.instancePath || '<root>'} ${e.message ?? ''}`.trim()).join('; ');
}

beforeAll(async () => {
    ajv = new Ajv2020({ allErrors: true, strict: false });
    addFormats(ajv);

    const methodsDir = join(SPEC_EXTENSION_DIR, 'methods');
    const entries = await readdir(methodsDir, { withFileTypes: true });
    for (const e of entries) {
        if (!e.isFile() || !e.name.endsWith('.schema.json')) continue;
        const schema = JSON.parse(await readFile(join(methodsDir, e.name), 'utf8')) as { $id?: string };
        const id = schema.$id ?? `urn:smooth-agent:sep:${e.name}`;
        if (!ajv.getSchema(id)) ajv.addSchema(schema, id);
        fileToId.set(e.name, id);
    }

    const raw = JSON.parse(await readFile(join(SPEC_EXTENSION_DIR, 'conformance', 'fixtures.json'), 'utf8')) as Record<
        string,
        unknown
    >;
    fixtures = Object.fromEntries(Object.entries(raw).filter(([k]) => !k.startsWith('$'))) as Record<string, Fixture>;
    invalidFixtures = raw.$invalid as InvalidFixture[];
});

describe('SEP extension conformance fixtures', () => {
    it('loaded every methods/*.schema.json', () => {
        expect(fileToId.size).toBeGreaterThanOrEqual(17);
    });

    it('loaded valid and invalid fixtures', () => {
        expect(Object.keys(fixtures).length).toBeGreaterThan(0);
        expect(invalidFixtures.length).toBeGreaterThan(0);
    });

    it('validates every valid fixture against its declared schema ref', () => {
        for (const [name, fixture] of Object.entries(fixtures)) {
            const result = validateAt(fixture.$schema_ref, fixture.instance);
            expect(result.valid, `${name} (${fixture.$schema_ref}): ${formatErrors(result.errors)}`).toBe(true);
        }
    });

    it('rejects every $invalid fixture', () => {
        for (const fixture of invalidFixtures) {
            const result = validateAt(fixture.$schema_ref, fixture.instance);
            expect(result.valid, `${fixture.name} (${fixture.$schema_ref}) unexpectedly validated`).toBe(false);
        }
    });

    it('rejects a known-good fixture mutated to violate its schema', () => {
        const fixture = fixtures.initialize_result!;
        const broken = structuredClone(fixture.instance) as { protocol_version: unknown };
        broken.protocol_version = 'not-an-integer';
        const result = validateAt(fixture.$schema_ref, broken);
        expect(result.valid).toBe(false);
        expect(result.errors.length).toBeGreaterThan(0);
    });
});
