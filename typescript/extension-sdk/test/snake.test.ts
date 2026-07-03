/**
 * The snake demo (Phase 8): the pure game core plus the render-block v2 widget
 * wiring — `play` pushes an interactive widget, and each `widget/key` advances
 * the game and re-renders.
 */
import { afterEach, describe, expect, it } from 'vitest';
import { createTestHost, type TestHost, type UiRequestParams } from '../src/index.js';
import { boardWidget, createSnake, newGame, step, type GameState } from '../examples/snake.js';

let host: TestHost | undefined;
afterEach(() => host?.close());

describe('snake game core (pure)', () => {
    it('steps the head in the current direction', () => {
        const s = newGame(); // head (3,4), dir right
        const next = step(s);
        expect(next.snake[0]).toEqual({ x: 4, y: 4 });
        expect(next.over).toBe(false);
    });

    it('turns on an arrow key but ignores a 180° reversal', () => {
        const s = newGame(); // moving right
        expect(step(s, 'ArrowLeft').snake[0]).toEqual({ x: 4, y: 4 }); // reversal ignored → keeps going right
        expect(step(s, 'ArrowUp').snake[0]).toEqual({ x: 3, y: 3 }); // turns up
    });

    it('eats food (grows + scores) and dies on a wall', () => {
        const eating: GameState = { snake: [{ x: 1, y: 0 }], dir: { x: 1, y: 0 }, food: { x: 2, y: 0 }, score: 0, over: false };
        const grown = step(eating);
        expect(grown.score).toBe(1);
        expect(grown.snake.length).toBe(2);

        const atEdge: GameState = { snake: [{ x: 11, y: 0 }], dir: { x: 1, y: 0 }, food: { x: 5, y: 5 }, score: 0, over: false };
        expect(step(atEdge).over).toBe(true);
    });

    it('boardWidget is a widget block with keybindings and a text fallback', () => {
        const w = boardWidget(newGame());
        expect(w.kind).toBe('widget');
        if (w.kind === 'widget') {
            expect(w.keybindings.map((k) => k.key)).toContain('ArrowUp');
            expect(w.keybindings.map((k) => k.key)).toContain('q');
            expect(typeof w.text).toBe('string');
        }
    });
});

describe('snake widget wiring', () => {
    it('play pushes a widget and each key re-renders', async () => {
        const widgets: UiRequestParams[] = [];
        host = createTestHost(createSnake(), {
            onUiRequest: (params) => {
                if (params.kind === 'set_widget') widgets.push(params);
                return {};
            },
        });
        await host.initialize({ mode: 'widget', ui_capabilities: ['set_widget'] });

        await host.callTool('play', {});
        expect(widgets.length).toBe(1);
        expect(widgets[0].kind === 'set_widget' && widgets[0].widget.kind).toBe('widget');

        host.sendEvent('widget/key', { widget_id: 'snake', key: 'ArrowDown' });
        await new Promise((r) => setTimeout(r, 0));
        expect(widgets.length).toBe(2); // the key drove a re-render
    });

    it('degrades on a host without set_widget', async () => {
        host = createTestHost(createSnake());
        await host.initialize(); // headless, no ui_capabilities
        const res = await host.callTool('play', {});
        expect(res.is_error).toBe(true);
    });
});
