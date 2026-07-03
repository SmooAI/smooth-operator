import type { ChatWidgetMode, ChatWidgetTheme } from './config.js';

/**
 * Render the widget's scoped stylesheet. All theme values are injected as CSS
 * custom properties on `:host` so they can be overridden per-instance and so the
 * styles below stay static. Kept deliberately framework-light — no Tailwind, no
 * runtime CSS-in-JS; just a string the web component drops into its shadow root.
 *
 * `mode` switches the host positioning + panel sizing between the floating
 * popover (default) and the full-page layout (fills its container/viewport).
 */
export function buildStyles(theme: Required<ChatWidgetTheme>, mode: ChatWidgetMode = 'popover'): string {
    return `
:host {
    --sac-text: ${theme.text};
    --sac-bg: ${theme.background};
    --sac-primary: ${theme.primary};
    --sac-primary-text: ${theme.primaryText};
    --sac-assistant-bubble: ${theme.assistantBubble};
    --sac-assistant-bubble-text: ${theme.assistantBubbleText};
    --sac-user-bubble: ${theme.userBubble};
    --sac-user-bubble-text: ${theme.userBubbleText};
    --sac-border: ${theme.border};

    ${
        mode === 'fullpage'
            ? `/* Full-page: fill the host's box (the element should be sized by its
       container, or it falls back to filling the viewport). */
    display: block;
    position: relative;
    width: 100%;
    height: 100%;
    min-height: 100vh;`
            : `/* Popover: float in the bottom-right corner. */
    position: fixed;
    bottom: 20px;
    right: 20px;
    z-index: 2147483000;`
    }
    font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, Helvetica, Arial, sans-serif;
}

* { box-sizing: border-box; }

.launcher {
    width: 56px;
    height: 56px;
    border-radius: 50%;
    border: none;
    cursor: pointer;
    background: var(--sac-primary);
    color: var(--sac-primary-text);
    box-shadow: 0 4px 16px rgba(0, 0, 0, 0.25);
    display: flex;
    align-items: center;
    justify-content: center;
    font-size: 24px;
    transition: transform 0.15s ease;
}
.launcher:hover { transform: scale(1.05); }

.panel {
    width: 360px;
    max-width: calc(100vw - 40px);
    height: 520px;
    max-height: calc(100vh - 40px);
    display: flex;
    flex-direction: column;
    background: var(--sac-bg);
    color: var(--sac-text);
    border: 1px solid var(--sac-border);
    border-radius: 14px;
    overflow: hidden;
    box-shadow: 0 12px 40px rgba(0, 0, 0, 0.35);
}

/* Full-page: the panel becomes the whole surface — no floating box, no shadow,
   no rounded corners; it fills the host. */
.panel.fullpage {
    width: 100%;
    height: 100%;
    min-height: 100vh;
    max-width: none;
    max-height: none;
    border: none;
    border-radius: 0;
    box-shadow: none;
}

.header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 12px 14px;
    background: var(--sac-primary);
    color: var(--sac-primary-text);
}
.header .brand { display: flex; align-items: center; gap: 10px; min-width: 0; }
.header .logo { height: 24px; width: auto; display: block; }
.header .title { font-weight: 600; font-size: 15px; }
.header .status { font-size: 11px; opacity: 0.85; }
.header .powered {
    font-size: 10px;
    opacity: 0.7;
    letter-spacing: 0.02em;
}
.header .close {
    background: transparent;
    border: none;
    color: inherit;
    cursor: pointer;
    font-size: 18px;
    line-height: 1;
    padding: 4px;
}

/* Full-page header: taller, logo-led, centered max-width content row. */
.panel.fullpage .header { padding: 14px 20px; }
.panel.fullpage .logo { height: 30px; }

.messages {
    flex: 1;
    overflow-y: auto;
    padding: 14px;
    display: flex;
    flex-direction: column;
    gap: 10px;
}

.bubble {
    max-width: 80%;
    padding: 9px 12px;
    border-radius: 12px;
    font-size: 14px;
    line-height: 1.4;
    white-space: pre-wrap;
    word-break: break-word;
}
.bubble.assistant {
    align-self: flex-start;
    background: var(--sac-assistant-bubble);
    color: var(--sac-assistant-bubble-text);
    border-bottom-left-radius: 4px;
}
.bubble.user {
    align-self: flex-end;
    background: var(--sac-user-bubble);
    color: var(--sac-user-bubble-text);
    border-bottom-right-radius: 4px;
}
.bubble.greeting { opacity: 0.85; font-style: italic; }

/* Full-page: center the conversation in a readable column and let bubbles
   breathe a little wider. */
.panel.fullpage .messages {
    padding: 24px 20px;
    align-items: stretch;
}
.panel.fullpage .messages > * {
    width: 100%;
    max-width: 760px;
    margin-left: auto;
    margin-right: auto;
}
.panel.fullpage .bubble { max-width: 100%; }
.panel.fullpage .bubble.user { align-self: flex-end; max-width: 80%; margin-right: auto; }
.panel.fullpage .bubble.assistant { align-self: flex-start; max-width: 100%; }

/* Sources panel — rendered under an assistant bubble whose terminal
   eventual_response carried citations. */
.prompt {
    align-self: flex-start;
    max-width: 80%;
    margin-top: -2px;
    display: flex;
    flex-direction: column;
    gap: 8px;
}
.panel.fullpage .prompt { max-width: 100%; }
.prompt-text { font-size: 13.5px; color: var(--sac-text); opacity: 0.9; }
.prompt-buttons { display: flex; gap: 8px; flex-wrap: wrap; }
.prompt-button {
    cursor: pointer;
    border: 1px solid var(--sac-border);
    background: var(--sac-bg);
    color: var(--sac-text);
    border-radius: 999px;
    padding: 6px 16px;
    font-size: 13px;
    font-weight: 600;
    transition: background 0.15s, color 0.15s, border-color 0.15s;
}
.prompt-button:hover {
    background: var(--sac-primary);
    color: var(--sac-primary-text);
    border-color: var(--sac-primary);
}
.prompt-answered { font-size: 12.5px; opacity: 0.7; font-style: italic; }

.sources {
    align-self: flex-start;
    max-width: 80%;
    margin-top: -4px;
    font-size: 12.5px;
    color: var(--sac-text);
}
.panel.fullpage .sources { max-width: 100%; }
.sources details { background: transparent; }
.sources summary {
    cursor: pointer;
    font-weight: 600;
    opacity: 0.85;
    list-style: none;
    user-select: none;
    padding: 2px 0;
}
.sources summary::-webkit-details-marker { display: none; }
.sources summary::before {
    content: '▸';
    display: inline-block;
    margin-right: 6px;
    transition: transform 0.15s ease;
}
.sources details[open] summary::before { transform: rotate(90deg); }
.sources ol {
    margin: 6px 0 0;
    padding-left: 0;
    list-style: none;
    display: flex;
    flex-direction: column;
    gap: 8px;
}
.sources li {
    border-left: 2px solid var(--sac-primary);
    padding-left: 10px;
}
.sources .src-title {
    color: var(--sac-primary);
    text-decoration: none;
    font-weight: 600;
    word-break: break-word;
}
.sources a.src-title:hover { text-decoration: underline; }
.sources span.src-title { color: var(--sac-text); opacity: 0.95; }
.sources .src-snippet {
    display: block;
    margin-top: 2px;
    opacity: 0.7;
    line-height: 1.4;
    white-space: normal;
}

.cursor::after {
    content: '▋';
    margin-left: 1px;
    animation: sac-blink 1s steps(2, start) infinite;
}
@keyframes sac-blink { to { visibility: hidden; } }

.composer {
    display: flex;
    gap: 8px;
    padding: 10px;
    border-top: 1px solid var(--sac-border);
}
.composer textarea {
    flex: 1;
    resize: none;
    border: 1px solid var(--sac-border);
    border-radius: 8px;
    padding: 8px 10px;
    font-family: inherit;
    font-size: 14px;
    background: transparent;
    color: var(--sac-text);
    max-height: 96px;
    line-height: 1.4;
}
.composer textarea:focus { outline: 1px solid var(--sac-primary); }
.composer button {
    border: none;
    border-radius: 8px;
    padding: 0 14px;
    cursor: pointer;
    background: var(--sac-primary);
    color: var(--sac-primary-text);
    font-weight: 600;
    font-size: 14px;
}
.composer button:disabled { opacity: 0.5; cursor: default; }

.hidden { display: none !important; }
`;
}
