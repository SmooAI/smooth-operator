/**
 * `todo` — the Phase 3 demo extension, pi's todo ported. A stateful checklist
 * whose tools drive the `ui/request` surface: every mutation pushes a
 * `set_widget` render block, and `clear` asks for a `confirm` first — both gated
 * on `hasUI`, so the same extension degrades cleanly on a headless host.
 *
 * State lives in-process (the extension is a long-lived subprocess, so the list
 * survives across tool calls within a session). Durable kv/appendEntry
 * persistence is Phase 4.
 *
 * Run it as a real SEP subprocess:  `tsx examples/todo.ts`
 */
import { z } from 'zod';
import { defineExtension, defineTool, type ToolContext } from '../src/index.js';

interface Item {
    text: string;
    done: boolean;
}

/** Build a fresh `todo` extension (its own empty list). Tests use this for
 * isolation; the served singleton below shares one list across a session. */
export const createTodo = () =>
    defineExtension((smooth) => {
        smooth.name = 'todo';
        smooth.version = '0.1.0';

        const items: Item[] = [];

    /** A `keyvalue` render block with a mandatory `text` fallback. */
    const widget = () => ({
        kind: 'keyvalue',
        title: 'Todos',
        rows: items.map((it, i) => ({ key: `${i + 1}`, value: `${it.done ? '✓' : '○'} ${it.text}` })),
        text: items.length ? items.map((it, i) => `${i + 1}. [${it.done ? 'x' : ' '}] ${it.text}`).join('\n') : '(no todos)',
    });

    const render = async (ctx: ToolContext) => {
        if (ctx.hasUI('set_widget')) await ctx.ui.setWidget(widget());
    };

    smooth.registerTool(
        defineTool<{ text: string }>({
            name: 'add',
            description: 'Add a todo item.',
            parameters: z.object({ text: z.string().describe('The task to add.') }),
            async execute(args, ctx) {
                items.push({ text: args.text, done: false });
                await render(ctx);
                return { content: `Added: ${args.text} (${items.length} total)` };
            },
        }),
    );

    smooth.registerTool(
        defineTool<{ index: number }>({
            name: 'done',
            description: 'Mark a todo done by its 1-based number.',
            parameters: z.object({ index: z.number().int().min(1).describe('1-based item number.') }),
            async execute(args, ctx) {
                const it = items[args.index - 1];
                if (!it) return { content: `No todo #${args.index}`, is_error: true };
                it.done = true;
                await render(ctx);
                return { content: `Done: ${it.text}` };
            },
        }),
    );

    smooth.registerTool(
        defineTool<Record<string, never>>({
            name: 'clear',
            description: 'Clear all todos (asks for confirmation when a UI is available).',
            parameters: z.object({}),
            async execute(_args, ctx) {
                if (ctx.hasUI('confirm')) {
                    const { confirmed, cancelled } = await ctx.ui.confirm(`Clear all ${items.length} todos?`);
                    if (cancelled || !confirmed) return { content: 'Cancelled.' };
                }
                const n = items.length;
                items.length = 0;
                await render(ctx);
                return { content: `Cleared ${n} todos.` };
            },
        }),
    );
    });

/** The served singleton — one shared list for the process's lifetime. */
export const todo = createTodo();

// When run directly (not imported by a test), serve over stdio.
if (import.meta.url === `file://${process.argv[1]}`) {
    todo.serve();
}
