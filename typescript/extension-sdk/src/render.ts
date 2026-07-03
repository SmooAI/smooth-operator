/**
 * Typed builders for the Phase 8 render-block DSL. These are thin — a render
 * block is plain data on the wire — but they keep an extension's UI code
 * type-checked and let a pi port replace `render*` functions mechanically.
 */

import type { Keybinding, RenderBlock } from './protocol.js';

export const render = {
    markdown: (text: string): RenderBlock => ({ kind: 'markdown', text }),

    keyvalue: (rows: { key: string; value: string }[], opts?: { title?: string; text?: string }): RenderBlock => ({
        kind: 'keyvalue',
        rows,
        ...(opts?.title !== undefined ? { title: opts.title } : {}),
        ...(opts?.text !== undefined ? { text: opts.text } : {}),
    }),

    table: (columns: string[], rows: string[][], opts?: { text?: string }): RenderBlock => ({
        kind: 'table',
        columns,
        rows,
        ...(opts?.text !== undefined ? { text: opts.text } : {}),
    }),

    diff: (patch: string, opts?: { text?: string }): RenderBlock => ({
        kind: 'diff',
        patch,
        ...(opts?.text !== undefined ? { text: opts.text } : {}),
    }),

    /** `value` is a 0..1 fraction. */
    progress: (value: number, opts?: { label?: string; text?: string }): RenderBlock => ({
        kind: 'progress',
        value,
        ...(opts?.label !== undefined ? { label: opts.label } : {}),
        ...(opts?.text !== undefined ? { text: opts.text } : {}),
    }),

    stack: (children: RenderBlock[], opts?: { text?: string }): RenderBlock => ({
        kind: 'stack',
        children,
        ...(opts?.text !== undefined ? { text: opts.text } : {}),
    }),

    /** The interactive tier: wraps `body` and declares the keys the host should
     *  route back as `widget/key` events. Correlate re-renders with `widgetId`. */
    widget: (widgetId: string, body: RenderBlock, keybindings: Keybinding[], opts?: { text?: string }): RenderBlock => ({
        kind: 'widget',
        widget_id: widgetId,
        body,
        keybindings,
        ...(opts?.text !== undefined ? { text: opts.text } : {}),
    }),
} as const;
