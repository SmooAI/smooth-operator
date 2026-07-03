/**
 * `plan-mode` — the Phase 4 flagship demo. One extension that exercises phases
 * 2–4 together:
 *
 * - **flag** (`--plan`): the host delivers it at `initialize`; the session starts
 *   in plan mode when set.
 * - **command** (`/plan`): toggles plan mode, with argument autocomplete
 *   (`on`/`off`).
 * - **tool_call intercept** (Phase 2 hook): while plan mode is on, mutating tools
 *   (write/edit/apply_patch/bash) are BLOCKED — the agent can read and think but
 *   not change the workspace.
 * - **widget** (Phase 3 ui): each toggle pushes a `set_widget` render block
 *   showing the current state (gated on `hasUI`, so it degrades headless).
 * - **appendEntry** (Phase 4 session): each toggle persists an LLM-invisible
 *   transcript entry, so the plan-mode history survives a hot reload of the
 *   extension (the host keeps the transcript; the flag re-establishes state).
 *
 * Run it as a real SEP subprocess:  `tsx examples/plan-mode.ts --plan`
 */
import { defineExtension } from '../src/index.js';

/** The tools plan mode blocks — anything that mutates the workspace. */
const WRITE_TOOLS = new Set(['write', 'edit', 'apply_patch', 'bash']);

/** Build a fresh `plan-mode` extension. Tests use this for isolation; the served
 * singleton below shares one state for the process's lifetime. */
export const createPlanMode = () =>
    defineExtension((smooth) => {
        smooth.name = 'plan-mode';
        smooth.version = '0.1.0';

        // null = follow the `--plan` flag; true/false once explicitly toggled.
        let toggled: boolean | null = null;
        const active = () => toggled ?? smooth.getFlag('plan') === true;

        smooth.registerFlag({ name: 'plan', description: 'Start the session in plan mode (file writes blocked).' });
        smooth.registerShortcut({ key: 'ctrl+p', command: 'plan', description: 'Toggle plan mode' });

        const widget = () => ({
            kind: 'keyvalue',
            title: 'Plan mode',
            rows: [{ key: 'status', value: active() ? 'ON — writes blocked' : 'off' }],
            text: `plan mode: ${active() ? 'ON' : 'off'}`,
        });

        smooth.registerCommand({
            name: 'plan',
            description: 'Toggle plan mode on/off (blocks file writes while on).',
            async execute(ctx) {
                const arg = String(ctx.args?.state ?? '').toLowerCase();
                toggled = arg === 'on' ? true : arg === 'off' ? false : !active();
                // Persist the toggle as an LLM-invisible entry (survives reload).
                await ctx.session.appendEntry({ kind: 'plan_mode', enabled: active() });
                if (ctx.hasUI('set_widget')) await ctx.ui.setWidget(widget());
                return { content: `Plan mode ${active() ? 'enabled — file writes are blocked' : 'disabled'}.` };
            },
            complete: (partial) => ['on', 'off'].filter((v) => v.startsWith(partial)).map((value) => ({ value })),
        });

        // Phase 2: veto mutating tool calls while plan mode is on.
        smooth.on('tool_call', (input) => {
            const tool = String((input as { tool?: string }).tool ?? '');
            if (active() && WRITE_TOOLS.has(tool)) {
                return { block: true, reason: `plan mode is on — \`${tool}\` is blocked. Toggle it off with /plan.` };
            }
        });

        // On a hot reload the host re-runs initialize (re-delivering `--plan`)
        // then fires session_start; re-render the widget for the fresh process.
        smooth.on('session_start', () => {
            if (smooth.hasUI('set_widget')) void smooth.ui.setWidget(widget());
        });
    });

/** The served singleton. */
export const planMode = createPlanMode();

// When run directly (not imported by a test), serve over stdio.
if (import.meta.url === `file://${process.argv[1]}`) {
    planMode.serve();
}
