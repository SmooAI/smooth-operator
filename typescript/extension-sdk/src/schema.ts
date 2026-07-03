/**
 * Turn a tool's declared `parameters` into the JSON Schema that goes on the wire.
 *
 * Three accepted shapes (the wire truth is always JSON Schema):
 * - a **zod v4** schema → converted with zod's built-in `z.toJSONSchema()`.
 * - a **TypeBox** schema → TypeBox schemas ARE JSON Schema, passed through.
 * - a **raw JSON Schema** object → passed through unchanged.
 */
import { z } from 'zod';

/** Anything acceptable as a tool's `parameters`. */
export type ParameterSchema = z.ZodType | Record<string, unknown>;

/** A zod v4 schema carries the internal `_zod` marker; nothing else we accept does. */
function isZodSchema(value: unknown): value is z.ZodType {
    return typeof value === 'object' && value !== null && '_zod' in value;
}

/** Normalize `schema` to a JSON Schema object (draft 2020-12 for zod). */
export function toJsonSchema(schema: ParameterSchema): Record<string, unknown> {
    if (isZodSchema(schema)) {
        // `io: 'input'` gives the schema the LLM should fill (pre-transform).
        return z.toJSONSchema(schema, { io: 'input' }) as Record<string, unknown>;
    }
    // TypeBox schemas and raw JSON Schema are already JSON Schema. Round-trip
    // through JSON to drop any symbol keys (TypeBox's `[Kind]`) that would never
    // survive the wire anyway.
    return JSON.parse(JSON.stringify(schema)) as Record<string, unknown>;
}
