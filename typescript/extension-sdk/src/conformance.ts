/**
 * `runConformance` — replay the shared SEP conformance fixtures against a REAL
 * extension subprocess. Where the schema-only conformance test (in the spec
 * repo) proves the fixtures match the schemas, this proves a live extension,
 * spawned and handshaken over stdio, answers each method with a schema-valid
 * reply. It is the SDK's dogfood gate and the template every polyglot SDK's
 * conformance runner follows.
 */
import { spawn } from 'node:child_process';
import { readFile, readdir } from 'node:fs/promises';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import _Ajv2020, { Ajv2020 as AjvClass, type ErrorObject } from 'ajv/dist/2020.js';
import _addFormats from 'ajv-formats';
import { Peer } from './jsonrpc.js';
import { PROTOCOL_VERSION, method } from './protocol.js';
import { stdioTransport } from './transport.js';

// ajv/ajv-formats ship CJS with a double-default under NodeNext; normalize both
// to the actual callable (same trick as the spec repo's validate.ts).
type Ajv = AjvClass;
const Ajv2020 = ((_Ajv2020 as unknown as { default?: unknown }).default ?? _Ajv2020) as typeof AjvClass;
const addFormats = ((_addFormats as unknown as { default?: unknown }).default ?? _addFormats) as (ajv: Ajv) => Ajv;

const __dirname = dirname(fileURLToPath(import.meta.url));
/** Repo-relative default: <repo>/spec/extension (works from src/ and dist/). */
export const DEFAULT_SPEC_DIR = join(__dirname, '..', '..', '..', 'spec', 'extension');

export interface ConformanceStep {
    name: string;
    ok: boolean;
    detail?: string;
}
export interface ConformanceReport {
    passed: boolean;
    steps: ConformanceStep[];
}

export interface RunConformanceOptions {
    command: string;
    args?: string[];
    env?: Record<string, string>;
    cwd?: string;
    /** Where spec/extension lives; defaults to the in-repo copy. */
    specDir?: string;
}

interface Fixture {
    $schema_ref: string;
    instance: unknown;
}

/** Load every methods/*.schema.json under `specDir` into one ajv instance. */
async function loadValidator(specDir: string): Promise<(ref: string, value: unknown) => ErrorObject[]> {
    const ajv = new Ajv2020({ allErrors: true, strict: false });
    addFormats(ajv);
    const fileToId = new Map<string, string>();
    const methodsDir = join(specDir, 'methods');
    for (const e of await readdir(methodsDir, { withFileTypes: true })) {
        if (!e.isFile() || !e.name.endsWith('.schema.json')) continue;
        const schema = JSON.parse(await readFile(join(methodsDir, e.name), 'utf8')) as { $id?: string };
        const id = schema.$id ?? `urn:sep:${e.name}`;
        if (!ajv.getSchema(id)) ajv.addSchema(schema, id);
        fileToId.set(e.name, id);
    }
    return (ref, value) => {
        const [path, pointer] = ref.split('#');
        const file = path!.split('/').pop()!;
        const id = fileToId.get(file);
        if (!id) throw new Error(`no schema for ref ${ref}`);
        const validate = ajv.getSchema(pointer ? `${id}#${pointer}` : id);
        if (!validate) throw new Error(`ajv could not resolve ${ref}`);
        return validate(value) ? [] : (validate.errors ?? []);
    };
}

function fmt(errors: ErrorObject[]): string {
    return errors.map((e) => `${e.instancePath || '<root>'} ${e.message ?? ''}`.trim()).join('; ');
}

/**
 * Spawn `command`, handshake, and replay the request/reply fixtures against the
 * live process, validating every reply against its `Result` schema. Resolves a
 * report; also returns non-zero via `passed: false` rather than throwing so a
 * caller can assert on the detail.
 */
export async function runConformance(opts: RunConformanceOptions): Promise<ConformanceReport> {
    const specDir = opts.specDir ?? DEFAULT_SPEC_DIR;
    const validate = await loadValidator(specDir);
    const fixtures = JSON.parse(await readFile(join(specDir, 'conformance', 'fixtures.json'), 'utf8')) as Record<string, Fixture>;

    const child = spawn(opts.command, opts.args ?? [], {
        stdio: ['pipe', 'pipe', 'inherit'],
        env: { ...process.env, ...opts.env },
        cwd: opts.cwd,
    });
    const transport = stdioTransport(child.stdout!, child.stdin!);
    const peer = new Peer({ send: (frame) => transport.send(frame) });
    peer.setNotificationHandler(method.TOOL_UPDATE, () => {});
    peer.setNotificationHandler(method.LOG, () => {});
    transport.start((frame) => peer.receive(frame));

    const steps: ConformanceStep[] = [];
    const check = async (name: string, requestMethod: string, params: unknown, resultRef: string) => {
        try {
            const result = await peer.request(requestMethod, params);
            const errors = validate(resultRef, result);
            steps.push({ name, ok: errors.length === 0, detail: errors.length ? fmt(errors) : undefined });
        } catch (err) {
            steps.push({ name, ok: false, detail: err instanceof Error ? err.message : String(err) });
        }
    };

    try {
        await check('initialize', method.INITIALIZE, initParams(fixtures), 'methods/initialize.schema.json#/$defs/Result');
        await check('ping', method.PING, {}, 'methods/ping.schema.json#/$defs/Result');
        await check('tool/execute', method.TOOL_EXECUTE, fixtures.tool_execute_params!.instance, 'methods/tool-execute.schema.json#/$defs/Result');
        await check('command/execute', method.COMMAND_EXECUTE, fixtures.command_execute_params!.instance, 'methods/command-execute.schema.json#/$defs/Result');
        await check('command/complete', method.COMMAND_COMPLETE, fixtures.command_complete_params!.instance, 'methods/command-complete.schema.json#/$defs/Result');
        await check('shutdown', method.SHUTDOWN, {}, 'methods/shutdown.schema.json#/$defs/Result');
    } finally {
        peer.close();
        transport.close();
        child.kill();
    }

    return { passed: steps.every((s) => s.ok), steps };
}

/** Handshake params from the fixture, pinned to the version this SDK speaks. */
function initParams(fixtures: Record<string, Fixture>): unknown {
    const base = fixtures.initialize_params!.instance as Record<string, unknown>;
    return { ...base, protocol_version: PROTOCOL_VERSION };
}
