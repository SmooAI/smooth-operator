/**
 * Theming via CSS custom properties.
 *
 * Every color/shape the components use is a `--smooth-*` CSS variable with a
 * default declared in `styles.css` (scoped to `.smooth-chat`, never `:root`, so
 * it can't leak into the host page). There are three ways to retheme, in order
 * of precedence (later wins):
 *
 *   1. **Your own CSS / Tailwind.** Override the variables on `.smooth-chat`
 *      (or any ancestor), e.g. in a Tailwind `@layer base` block:
 *        .smooth-chat { --smooth-color-primary: theme(colors.indigo.600); }
 *      Because they're plain CSS variables, `theme()`, design tokens, and
 *      light/dark media queries all just work — no build coupling to this package.
 *
 *   2. **A `theme` prop** on `<SmoothChat>` (or `themeToStyle(theme)` spread onto
 *      any element you render the hook into). This sets the variables inline, so
 *      it wins over stylesheet rules and is the easiest per-instance override.
 *
 *   3. **Inline `style`** on the element, for one-off tweaks.
 *
 * Keys intentionally match `@smooai/chat-widget`'s `ChatWidgetTheme` so a brand
 * palette ports between the web-component widget and these React components
 * unchanged.
 */
import type { CSSProperties } from 'react';

export interface ChatTheme {
    /** Foreground text color. → `--smooth-color-text` */
    text?: string;
    /** Outer surface / page background. → `--smooth-color-bg` */
    background?: string;
    /** Panel (message column) background. → `--smooth-color-surface` */
    surface?: string;
    /** Primary accent (header, send button, user bubble). → `--smooth-color-primary` */
    primary?: string;
    /** Text rendered on top of `primary`. → `--smooth-color-primary-text` */
    primaryText?: string;
    /** Assistant bubble background. → `--smooth-color-assistant-bubble` */
    assistantBubble?: string;
    /** Assistant bubble text. → `--smooth-color-assistant-bubble-text` */
    assistantBubbleText?: string;
    /** User bubble background (defaults to `primary`). → `--smooth-color-user-bubble` */
    userBubble?: string;
    /** User bubble text (defaults to `primaryText`). → `--smooth-color-user-bubble-text` */
    userBubbleText?: string;
    /** Border / divider color. → `--smooth-color-border` */
    border?: string;
    /** Secondary / muted text (status, snippets). → `--smooth-color-muted` */
    muted?: string;
    /** Corner radius for bubbles, inputs, the panel. → `--smooth-radius` */
    radius?: string;
    /** Font family for the whole surface. → `--smooth-font` */
    fontFamily?: string;
}

/** Map each `ChatTheme` key to its CSS custom property name. */
const VAR_BY_KEY: Record<keyof ChatTheme, string> = {
    text: '--smooth-color-text',
    background: '--smooth-color-bg',
    surface: '--smooth-color-surface',
    primary: '--smooth-color-primary',
    primaryText: '--smooth-color-primary-text',
    assistantBubble: '--smooth-color-assistant-bubble',
    assistantBubbleText: '--smooth-color-assistant-bubble-text',
    userBubble: '--smooth-color-user-bubble',
    userBubbleText: '--smooth-color-user-bubble-text',
    border: '--smooth-color-border',
    muted: '--smooth-color-muted',
    radius: '--smooth-radius',
    fontFamily: '--smooth-font',
};

/**
 * Turn a {@link ChatTheme} into a `style` object of CSS custom properties.
 * Only keys you set are emitted; everything else falls back to the defaults in
 * `styles.css`. Spread it onto the root element:
 *
 * ```tsx
 * <div className="smooth-chat" style={themeToStyle({ primary: '#4f46e5' })}>…</div>
 * ```
 */
export function themeToStyle(theme: ChatTheme | undefined): CSSProperties {
    const style: Record<string, string> = {};
    if (!theme) return style as CSSProperties;
    for (const key of Object.keys(theme) as (keyof ChatTheme)[]) {
        const value = theme[key];
        if (typeof value === 'string' && value.length > 0) {
            style[VAR_BY_KEY[key]] = value;
        }
    }
    return style as CSSProperties;
}
