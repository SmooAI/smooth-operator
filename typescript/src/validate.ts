/**
 * Runtime validation against the spec JSON Schemas, using ajv.
 *
 * The spec ships draft 2020-12 schemas with internal `$defs` (no cross-file
 * `$ref`s). We register every schema under its `$id` so ajv can resolve any
 * `#/$defs/...` pointer within it, then expose:
 *
 *  - `validateAt(schemaRef, instance)` — validate against a spec-relative ref like
 *    `events/stream-chunk.schema.json` or `actions/send-message.schema.json#/$defs/Request`
 *    (the exact form used by `conformance/fixtures.json`).
 *  - `validateEvent` / `validateAction` — convenience validators that pick the
 *    right schema from a frame's discriminator and validate it.
 *
 * Schemas are loaded from the spec directory on disk. This module is Node-only
 * (it reads files); it is intended for build/test/server use, not the browser
 * bundle. The wire client (`client.ts`) does not import it — validation is opt-in.
 */
import _Ajv2020, { Ajv2020 as AjvClass, type ValidateFunction, type ErrorObject as AjvError } from 'ajv/dist/2020.js';
import _addFormats from 'ajv-formats';

// ajv (and ajv-formats) ship as CJS with a double-default under NodeNext, so the
// runtime constructor can be nested one level deeper than the imported binding.
// Normalize both to the actual callable. `AjvClass` (named export) gives us a
// usable *type*; the runtime value comes from the default's `.default` if present.
type Ajv = AjvClass;
const Ajv2020 = ((_Ajv2020 as unknown as { default?: unknown }).default ?? _Ajv2020) as typeof AjvClass;
const addFormats = ((_addFormats as unknown as { default?: unknown }).default ?? _addFormats) as (ajv: Ajv) => Ajv;
import { readFile, readdir } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import type { ActionType, EventType } from './types.js';

const __dirname = dirname(fileURLToPath(import.meta.url));

/** Default location of the spec dir relative to this source file (../../spec). */
export const DEFAULT_SPEC_DIR = join(__dirname, '..', '..', 'spec');

/** Maps an event `type` to its schema file (spec-relative). */
const EVENT_SCHEMA_FILE: Record<EventType, string> = {
    immediate_response: 'events/immediate-response.schema.json',
    eventual_response: 'events/eventual-response.schema.json',
    stream_chunk: 'events/stream-chunk.schema.json',
    stream_token: 'events/stream-token.schema.json',
    keepalive: 'events/keepalive.schema.json',
    write_confirmation_required: 'events/write-confirmation-required.schema.json',
    otp_verification_required: 'events/otp-verification-required.schema.json',
    otp_sent: 'events/otp-sent.schema.json',
    otp_verified: 'events/otp-verified.schema.json',
    otp_invalid: 'events/otp-invalid.schema.json',
    identity_intake_required: 'events/identity-intake-required.schema.json',
    identity_intake_invalid: 'events/identity-intake-invalid.schema.json',
    error: 'events/error.schema.json',
    pong: 'events/pong.schema.json',
};

/** Maps an action `action` to its request schema ref (spec-relative). */
const ACTION_SCHEMA_REF: Record<ActionType, string> = {
    create_conversation_session: 'actions/create-conversation-session.schema.json#/$defs/Request',
    send_message: 'actions/send-message.schema.json#/$defs/Request',
    get_session: 'actions/get-session.schema.json#/$defs/Request',
    get_conversation_messages: 'actions/get-messages.schema.json#/$defs/Request',
    confirm_tool_action: 'actions/confirm-tool-action.schema.json#/$defs/Request',
    verify_otp: 'actions/verify-otp.schema.json#/$defs/Request',
    submit_identity_intake: 'actions/submit-identity-intake.schema.json#/$defs/Request',
    ping: 'actions/ping.schema.json#/$defs/Request',
};

export interface ValidationResult {
    valid: boolean;
    errors: AjvError[];
}

export class ProtocolValidator {
    private readonly ajv: Ajv;
    /** spec-relative file path → the schema's `$id` (used to build `$ref`-able URIs). */
    private readonly fileToId = new Map<string, string>();
    private readonly cache = new Map<string, ValidateFunction>();

    private constructor(ajv: Ajv) {
        this.ajv = ajv;
    }

    /** Load every `*.schema.json` under `specDir` and register it with ajv. */
    static async load(specDir: string = DEFAULT_SPEC_DIR): Promise<ProtocolValidator> {
        const ajv = new Ajv2020({ allErrors: true, strict: false });
        addFormats(ajv);

        const validator = new ProtocolValidator(ajv);

        for (const sub of ['', 'actions', 'events', 'domain']) {
            const dir = sub ? join(specDir, sub) : specDir;
            const entries = await readdir(dir, { withFileTypes: true });
            for (const e of entries) {
                if (!e.isFile() || !e.name.endsWith('.schema.json')) continue;
                const rel = sub ? `${sub}/${e.name}` : e.name;
                const schema = JSON.parse(await readFile(join(dir, e.name), 'utf8')) as { $id?: string };
                const id = schema.$id ?? `urn:smooth-agent:${rel}`;
                // ajv throws if the same $id is added twice; guard against it.
                if (!ajv.getSchema(id)) ajv.addSchema(schema, id);
                validator.fileToId.set(rel, id);
            }
        }

        return validator;
    }

    /**
     * Validate `instance` against a spec-relative schema ref. The ref is the form
     * used in `fixtures.json`: a file path, optionally with a JSON-pointer fragment
     * into the schema's `$defs` (e.g. `actions/ping.schema.json#/$defs/Request`).
     */
    validateAt(schemaRef: string, instance: unknown): ValidationResult {
        const validate = this.compile(schemaRef);
        const valid = validate(instance) as boolean;
        return { valid, errors: valid ? [] : (validate.errors ?? []) };
    }

    /** Validate a server event frame by selecting the schema from its `type`. */
    validateEvent(frame: { type: EventType } & Record<string, unknown>): ValidationResult {
        const file = EVENT_SCHEMA_FILE[frame.type];
        if (!file) {
            return { valid: false, errors: [syntheticError(`Unknown event type: ${String(frame.type)}`)] };
        }
        return this.validateAt(file, frame);
    }

    /** Validate a client action frame by selecting the schema from its `action`. */
    validateAction(frame: { action: ActionType } & Record<string, unknown>): ValidationResult {
        const ref = ACTION_SCHEMA_REF[frame.action];
        if (!ref) {
            return { valid: false, errors: [syntheticError(`Unknown action: ${String(frame.action)}`)] };
        }
        return this.validateAt(ref, frame);
    }

    private compile(schemaRef: string): ValidateFunction {
        const cached = this.cache.get(schemaRef);
        if (cached) return cached;

        const [file, pointer] = schemaRef.split('#');
        const id = this.fileToId.get(file!);
        if (!id) throw new Error(`No schema registered for "${file}" (ref "${schemaRef}")`);

        // Resolve via the registered $id + optional JSON-pointer fragment.
        const uri = pointer ? `${id}#${pointer}` : id;
        const validate = this.ajv.getSchema(uri);
        if (!validate) throw new Error(`ajv could not resolve schema ref "${schemaRef}" (uri "${uri}")`);

        this.cache.set(schemaRef, validate);
        return validate;
    }
}

function syntheticError(message: string): AjvError {
    return { instancePath: '', schemaPath: '', keyword: 'protocol', params: {}, message };
}

/** Render ajv errors into a single human-readable string. */
export function formatErrors(errors: AjvError[]): string {
    return errors.map((e) => `${e.instancePath || '<root>'} ${e.message ?? ''}`.trim()).join('; ');
}
