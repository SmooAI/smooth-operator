/** `toJsonSchema` accepts zod v4, raw JSON Schema, and TypeBox-shaped schemas. */
import { describe, expect, it } from 'vitest';
import { z } from 'zod';
import { toJsonSchema } from '../src/index.js';

describe('toJsonSchema', () => {
    it('converts a zod v4 object schema to JSON Schema', () => {
        const js = toJsonSchema(z.object({ phrase: z.string(), n: z.number().optional() }));
        expect(js).toMatchObject({ type: 'object', properties: { phrase: { type: 'string' } }, required: ['phrase'] });
    });

    it('passes raw JSON Schema through unchanged', () => {
        const raw = { type: 'object', properties: { x: { type: 'boolean' } }, required: ['x'] };
        expect(toJsonSchema(raw)).toEqual(raw);
    });

    it('strips symbol keys (TypeBox `[Kind]`) so the result is wire-clean JSON', () => {
        const typeboxLike: Record<string | symbol, unknown> = { type: 'string', minLength: 1 };
        typeboxLike[Symbol.for('TypeBox.Kind')] = 'String';
        const js = toJsonSchema(typeboxLike as Record<string, unknown>);
        expect(js).toEqual({ type: 'string', minLength: 1 });
        expect(Object.getOwnPropertySymbols(js)).toHaveLength(0);
    });
});
