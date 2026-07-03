/**
 * `snake` — the Phase 8 headline demo: pi's snake game, ported to the SEP
 * render-block v2 interactive-widget DSL.
 *
 * The `play` tool seeds a game and pushes a `widget`-kind render block declaring
 * arrow-key + `q` keybindings. The host routes each matching keypress back as a
 * `widget/key` event; the handler advances the game and re-renders via
 * `ui.setWidget`. Same widget block, two fidelities:
 *   - **web** renders the `stack` (grid + score + speed bar) full-fidelity;
 *   - **TUI** renders the same block reduced (the ASCII grid + score text).
 * The keybinding DSL is identical across both — that is the whole point.
 *
 * Run it as a real SEP subprocess:  `tsx examples/snake.ts`
 */
import { z } from 'zod';
import { defineExtension, defineTool, render, type RenderBlock, type ToolContext, type WidgetKeyPayload } from '../src/index.js';

const WIDTH = 12;
const HEIGHT = 8;

export type Point = { x: number; y: number };
export interface GameState {
    snake: Point[]; // head first
    dir: Point;
    food: Point;
    score: number;
    over: boolean;
}

const KEY_DIRS: Record<string, Point> = {
    ArrowUp: { x: 0, y: -1 },
    ArrowDown: { x: 0, y: 1 },
    ArrowLeft: { x: -1, y: 0 },
    ArrowRight: { x: 1, y: 0 },
};

/** Deterministic food placement: the first free cell in row-major order (keeps
 *  the game unit-testable; a real game would randomize). */
function placeFood(snake: Point[]): Point {
    for (let y = 0; y < HEIGHT; y++) {
        for (let x = 0; x < WIDTH; x++) {
            if (!snake.some((s) => s.x === x && s.y === y)) return { x, y };
        }
    }
    return { x: 0, y: 0 };
}

export function newGame(): GameState {
    const snake = [{ x: 3, y: 4 }];
    return { snake, dir: { x: 1, y: 0 }, food: placeFood(snake), score: 0, over: false };
}

/** Advance one tick. A direction key turns (ignoring a 180° reversal) then steps;
 *  any other key just steps. Pure — no I/O — so the game logic tests standalone. */
export function step(state: GameState, key?: string): GameState {
    if (state.over) return state;
    let dir = state.dir;
    const turn = key ? KEY_DIRS[key] : undefined;
    if (turn && !(turn.x === -dir.x && turn.y === -dir.y)) dir = turn;

    const head = { x: state.snake[0].x + dir.x, y: state.snake[0].y + dir.y };
    const hitsWall = head.x < 0 || head.x >= WIDTH || head.y < 0 || head.y >= HEIGHT;
    const hitsSelf = state.snake.some((s) => s.x === head.x && s.y === head.y);
    if (hitsWall || hitsSelf) return { ...state, dir, over: true };

    const ate = head.x === state.food.x && head.y === state.food.y;
    const snake = [head, ...state.snake];
    if (!ate) snake.pop();
    return {
        snake,
        dir,
        food: ate ? placeFood(snake) : state.food,
        score: state.score + (ate ? 1 : 0),
        over: false,
    };
}

/** Render the ASCII grid for the widget body + the `text` fallback. */
function grid(state: GameState): string {
    const rows: string[] = [];
    for (let y = 0; y < HEIGHT; y++) {
        let row = '';
        for (let x = 0; x < WIDTH; x++) {
            if (state.snake[0].x === x && state.snake[0].y === y) row += '@';
            else if (state.snake.some((s) => s.x === x && s.y === y)) row += 'o';
            else if (state.food.x === x && state.food.y === y) row += '*';
            else row += '.';
        }
        rows.push(row);
    }
    return rows.join('\n');
}

/** The interactive widget render block: a `stack` body (grid + score + speed)
 *  plus the arrow/`q` keybindings the host routes back as `widget/key`. */
export function boardWidget(state: GameState): RenderBlock {
    const board = '```\n' + grid(state) + '\n```';
    const body = render.stack([
        render.markdown(state.over ? `Game over! Final score ${state.score}. Press q to quit.` : `Score ${state.score}`),
        render.markdown(board),
        render.progress(Math.min(1, state.snake.length / (WIDTH * HEIGHT)), { label: 'length' }),
    ]);
    return render.widget('snake', body, [
        { key: 'ArrowUp', description: 'up' },
        { key: 'ArrowDown', description: 'down' },
        { key: 'ArrowLeft', description: 'left' },
        { key: 'ArrowRight', description: 'right' },
        { key: 'q', description: 'quit' },
    ], { text: `snake — score ${state.score}\n${grid(state)}` });
}

export const createSnake = () =>
    defineExtension((smooth) => {
        smooth.name = 'snake';
        smooth.version = '0.1.0';

        let state = newGame();

        smooth.registerTool(
            defineTool({
                name: 'play',
                description: 'Start a game of snake. Use the arrow keys; press q to quit.',
                parameters: z.object({}),
                async execute(_args, ctx: ToolContext) {
                    state = newGame();
                    if (!ctx.hasUI('set_widget')) {
                        return { content: 'snake needs a widget-capable frontend (set_widget).', is_error: true };
                    }
                    await ctx.ui.setWidget(boardWidget(state));
                    return { content: 'Playing snake — arrow keys to steer, q to quit.' };
                },
            }),
        );

        // Each keypress the host routes to our widget advances the game and
        // re-renders. `q` ends it.
        smooth.on('widget/key', async (payload) => {
            const { key } = (payload ?? {}) as WidgetKeyPayload;
            if (key === 'q') {
                state = { ...state, over: true };
            } else {
                state = step(state, key);
            }
            await smooth.ui.setWidget(boardWidget(state));
        });
    });

export const snake = createSnake();

if (import.meta.url === `file://${process.argv[1]}`) {
    snake.serve();
}
