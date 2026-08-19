/**
 * Drift guard over the hand-maintained dispatch unions in `src/types.ts`.
 *
 * `conformance.test.ts` validates fixtures against the schemas but never checks that
 * the client's own `EVENT_TYPES` / `ACTION_TYPES` cover them — which is how
 * `stream_reasoning` ended up missing from the union while the generated type,
 * the production Rust server emitter, and a `case 'stream_reasoning'` in the
 * web-chat example all existed. `isServerEvent()` returns false for an unlisted
 * type, and `handleFrame` drops such frames before they reach a turn.
 *
 * The expected sets are derived from `spec/` at test time, never from a list
 * maintained here — a guard asserting against its own copy of the constant would
 * lock the drift in instead of catching it.
 */
import { readFile, readdir } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import { describe, expect, it } from 'vitest';
import { ACTION_TYPES, EVENT_TYPES, isServerEvent, isClientAction } from '../src/types.js';

const __dirname = dirname(fileURLToPath(import.meta.url));
const SPEC_DIR = join(__dirname, '..', '..', 'spec');

/** The `const` value of the named discriminator property in every schema in a dir. */
async function discriminators(subdir: string, prop: 'type' | 'action'): Promise<string[]> {
    const dir = join(SPEC_DIR, subdir);
    const files = (await readdir(dir)).filter((f) => f.endsWith('.schema.json'));
    const out: string[] = [];
    for (const file of files) {
        const schema = JSON.parse(await readFile(join(dir, file), 'utf8')) as Record<string, any>;
        // Action schemas nest the frame under $defs/Request; event schemas are top-level.
        const root = schema.$defs?.Request ?? schema;
        const value = root?.properties?.[prop]?.const;
        if (typeof value === 'string') out.push(value);
    }
    return out;
}

describe('dispatch unions cover the spec', () => {
    it('EVENT_TYPES covers every schema in spec/events', async () => {
        const specEvents = await discriminators('events', 'type');
        expect(specEvents.length).toBeGreaterThan(0);

        const missing = specEvents.filter((t) => !(EVENT_TYPES as readonly string[]).includes(t));
        expect(
            missing,
            `spec/events declares [${missing.join(', ')}] but EVENT_TYPES omits them: ` +
                'isServerEvent() rejects the frame and handleFrame drops it silently',
        ).toEqual([]);
    });

    it('ACTION_TYPES covers every schema in spec/actions', async () => {
        const specActions = await discriminators('actions', 'action');
        expect(specActions.length).toBeGreaterThan(0);

        const missing = specActions.filter((a) => !(ACTION_TYPES as readonly string[]).includes(a));
        expect(missing, `spec/actions declares [${missing.join(', ')}] but ACTION_TYPES omits them`).toEqual([]);
    });

    it('isServerEvent accepts a minimal frame for every spec event type', async () => {
        const specEvents = await discriminators('events', 'type');
        const rejected = specEvents.filter((type) => !isServerEvent({ type, data: {} }));
        expect(rejected, `isServerEvent() would drop these frame types: [${rejected.join(', ')}]`).toEqual([]);
    });

    it('isClientAction accepts a minimal frame for every spec action', async () => {
        const specActions = await discriminators('actions', 'action');
        const rejected = specActions.filter((action) => !isClientAction({ action }));
        expect(rejected, `isClientAction() would reject these actions: [${rejected.join(', ')}]`).toEqual([]);
    });
});
