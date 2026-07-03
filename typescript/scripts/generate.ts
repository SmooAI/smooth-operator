/**
 * Codegen: emit `src/generated/types.ts` from the language-neutral JSON Schemas
 * in `../spec`.
 *
 * Strategy
 * --------
 * Every schema file in the spec is self-contained: all `$ref`s point at internal
 * `#/$defs/...` definitions (verified — no cross-file refs). So we can run
 * `json-schema-to-typescript` per file and concatenate the results into a single
 * `types.ts`, deduplicating shared interface names that several files happen to
 * declare (e.g. `ErrorObject`).
 *
 * The envelope file is special: it is a top-level `oneOf` of `ActionEnvelope` and
 * `EventEnvelope` with the interesting shapes living under `$defs`. We compile its
 * `$defs` so consumers get `ActionEnvelope` / `EventEnvelope` / `ErrorObject`.
 *
 * The action/event/domain files likewise hang their real shapes off `$defs`
 * (Request/Response) or are a flat top-level object (events). We feed each file's
 * effective schema to json-schema-to-typescript with a stable title so the emitted
 * interface name is predictable.
 */
import { readFile, writeFile, readdir } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { dirname, join, relative } from 'node:path';
import { compile, type JSONSchema } from 'json-schema-to-typescript';

const __dirname = dirname(fileURLToPath(import.meta.url));
const SPEC_DIR = join(__dirname, '..', '..', 'spec');
const OUT_FILE = join(__dirname, '..', 'src', 'generated', 'types.ts');

const COMPILE_OPTS = {
    additionalProperties: false,
    bannerComment: '',
    style: { singleQuote: true, tabWidth: 4, trailingComma: 'all' as const },
    declareExternallyReferenced: true,
    enableConstEnums: false,
};

async function listSchemas(): Promise<string[]> {
    const out: string[] = [];
    for (const sub of ['', 'actions', 'events', 'domain', 'interactions']) {
        const dir = sub ? join(SPEC_DIR, sub) : SPEC_DIR;
        const entries = await readdir(dir, { withFileTypes: true });
        for (const e of entries) {
            if (e.isFile() && e.name.endsWith('.schema.json')) {
                out.push(join(dir, e.name));
            }
        }
    }
    return out.sort();
}

/**
 * For a file whose top-level is a `oneOf` over `$defs` (envelope, actions), strip
 * the `oneOf`/`$ref` wrapper and return one synthetic schema per named `$def` so
 * each gets its own exported interface. Flat object files are returned as-is.
 */
function expand(schema: JSONSchema, fileTitle: string): JSONSchema[] {
    const defs = (schema.$defs ?? {}) as Record<string, JSONSchema>;
    const hasOneOf = Array.isArray(schema.oneOf);

    if (hasOneOf && Object.keys(defs).length > 0) {
        // Emit each named $def as a standalone schema, carrying $defs along so
        // internal refs still resolve.
        return Object.entries(defs).map(([name, def]) => ({
            ...def,
            $defs: defs,
            title: def.title ?? `${fileTitle}${name}`,
        }));
    }

    // Flat top-level object (the event schemas, domain schemas).
    return [{ ...schema, title: schema.title ?? fileTitle }];
}

async function main(): Promise<void> {
    const files = await listSchemas();
    const seenNames = new Set<string>();
    const blocks: string[] = [];

    for (const file of files) {
        const raw = JSON.parse(await readFile(file, 'utf8')) as JSONSchema;
        const fileTitle = String(raw.title ?? '');
        const subSchemas = expand(raw, fileTitle);

        for (const sub of subSchemas) {
            // json-schema-to-typescript keys the top-level interface name off `title`.
            const ts = await compile(sub, String(sub.title ?? fileTitle), COMPILE_OPTS);
            // Drop interfaces whose name we've already emitted from an earlier file
            // (shared defs like ErrorObject appear in multiple files).
            const filtered = stripOrphanComments(dropDuplicateDeclarations(ts, seenNames));
            if (filtered.trim().length > 0) {
                blocks.push(`// ── from ${relative(SPEC_DIR, file)} ──\n${filtered.trim()}\n`);
            }
        }
    }

    const header = `/**
 * AUTO-GENERATED — do not edit by hand.
 *
 * Generated from the JSON Schemas in ../spec by scripts/generate.ts
 * Run \`pnpm generate\` to regenerate after a schema change.
 */
/* eslint-disable */

`;

    await writeFile(OUT_FILE, header + blocks.join('\n'), 'utf8');
    console.log(`Wrote ${OUT_FILE} (${blocks.length} declaration blocks from ${files.length} schema files).`);
}

/**
 * json-schema-to-typescript emits each interface/type as a top-level declaration.
 * Walk the emitted source, drop any declaration whose name we have already seen,
 * and register newly-seen names. This deduplicates shared `$defs` (e.g.
 * `ErrorObject`, `ConversationMessage`) that several files compile independently.
 */
function dropDuplicateDeclarations(source: string, seen: Set<string>): string {
    const declRe = /^export (?:interface|type) (\w+)\b/;
    const lines = source.split('\n');
    const kept: string[] = [];

    let skipping = false;
    let depth = 0;
    let currentIsType = false;

    for (const line of lines) {
        const m = line.match(declRe);
        if (m && depth === 0) {
            const name = m[1]!;
            currentIsType = line.startsWith('export type');
            if (seen.has(name)) {
                skipping = true;
            } else {
                seen.add(name);
                skipping = false;
            }
        }

        if (!skipping) kept.push(line);

        // Track brace depth so we know when an interface block ends.
        for (const ch of line) {
            if (ch === '{') depth++;
            else if (ch === '}') depth = Math.max(0, depth - 1);
        }

        // `export type X = ...;` declarations are single-line (depth stays 0) —
        // stop skipping once the statement terminates.
        if (currentIsType && depth === 0 && line.includes(';')) {
            skipping = false;
            currentIsType = false;
        } else if (!currentIsType && depth === 0 && skipping && line.includes('}')) {
            skipping = false;
        }
    }

    return kept.join('\n');
}

/**
 * After dropping a duplicate declaration we may be left with its leading JSDoc
 * block dangling with no declaration after it. Remove any `/** ... *​/` comment
 * that is not immediately followed (ignoring blank lines) by a declaration.
 */
function stripOrphanComments(source: string): string {
    return source.replace(/\/\*\*[\s\S]*?\*\/\s*(?=\n\s*(?:\/\*\*|$))/g, '').replace(/\n{3,}/g, '\n\n');
}

main().catch((err) => {
    console.error(err);
    process.exit(1);
});
