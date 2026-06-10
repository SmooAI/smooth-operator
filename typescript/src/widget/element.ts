/**
 * `<smooth-agent-chat>` — a framework-light embeddable chat web component.
 *
 * Ported (and simplified) from smooai's `@smooai/ui-chat-widget`. The original is a
 * React custom element that mounts the heavyweight `@smooai/ui` ChatWidget and
 * pulls in the whole monorepo (Tailwind, shadcn, react-phone-number-input, MSW,
 * Supabase auth …). This is a clean, dependency-light rewrite that preserves the
 * embedding model — a custom element with a launcher + popover panel, declarative
 * HTML attributes, and a programmatic API — while talking to the
 * `@smooai/smooth-operator` protocol client instead of `@smooai/realtime`.
 *
 * Embedding model:
 *   <smooth-agent-chat endpoint="ws://localhost:8787/ws" agent-id="…"></smooth-agent-chat>
 * or programmatically via {@link mountChatWidget}.
 */
import type { ChatWidgetConfig, ChatWidgetMode, ChatWidgetTheme } from './config.js';
import { resolveConfig } from './config.js';
import { type ChatMessage, type Citation, type ConnectionStatus, ConversationController } from './conversation.js';
import { SMOOTH_LOGO_SVG } from './logo.js';
import { buildStyles } from './styles.js';

export const ELEMENT_TAG = 'smooth-agent-chat';

const OBSERVED = ['endpoint', 'agent-id', 'agent-name', 'placeholder', 'greeting', 'start-open', 'mode'] as const;

/**
 * Return `url` only if it is a valid absolute `http(s)` URL, else `null`.
 *
 * SECURITY: citation URLs originate from indexed content (web / GitHub
 * connectors), which can be attacker-influenceable. Assigning an arbitrary
 * string to `<a>.href` allows `javascript:`/`data:`/`vbscript:` URLs that
 * execute on click — a stored-XSS vector. Only http(s) links are rendered as
 * anchors; anything else falls back to plain text.
 */
export function safeHttpUrl(url: string | undefined | null): string | null {
    if (!url) return null;
    try {
        const parsed = new URL(url);
        return parsed.protocol === 'http:' || parsed.protocol === 'https:' ? parsed.href : null;
    } catch {
        return null;
    }
}

export class SmoothAgentChatElement extends HTMLElement {
    static get observedAttributes(): readonly string[] {
        return OBSERVED;
    }

    private readonly root: ShadowRoot;
    private controller: ConversationController | null = null;
    private overrides: Partial<ChatWidgetConfig> = {};
    private open = false;
    private messages: ChatMessage[] = [];
    private status: ConnectionStatus = 'idle';
    private mounted = false;

    // Cached DOM refs (populated in render()).
    private panelEl: HTMLElement | null = null;
    private launcherEl: HTMLElement | null = null;
    private messagesEl: HTMLElement | null = null;
    private statusEl: HTMLElement | null = null;
    private inputEl: HTMLTextAreaElement | null = null;
    private sendBtn: HTMLButtonElement | null = null;

    constructor() {
        super();
        this.root = this.attachShadow({ mode: 'open' });
    }

    connectedCallback(): void {
        this.mounted = true;
        this.render();
    }

    disconnectedCallback(): void {
        this.mounted = false;
        this.controller?.disconnect();
        this.controller = null;
    }

    attributeChangedCallback(): void {
        if (this.mounted) this.render();
    }

    /**
     * Programmatically merge config overrides (endpoint, agentId, theme, …). Values
     * set here take precedence over HTML attributes. Re-renders the widget.
     */
    configure(config: Partial<ChatWidgetConfig>): void {
        this.overrides = { ...this.overrides, ...config };
        if (config.theme) {
            this.overrides.theme = { ...(this.overrides.theme ?? {}), ...config.theme };
        }
        if (this.mounted) this.render();
    }

    /** Open the chat panel. */
    openChat(): void {
        this.open = true;
        this.syncOpenState();
        void this.controller?.connect().catch(() => {});
    }

    /** Collapse the chat panel back to the launcher. */
    closeChat(): void {
        this.open = false;
        this.syncOpenState();
    }

    // ─────────────────────────── Config resolution ─────────────────────────────

    private readConfig(): ChatWidgetConfig | null {
        const endpoint = this.overrides.endpoint ?? this.getAttribute('endpoint') ?? '';
        const agentId = this.overrides.agentId ?? this.getAttribute('agent-id') ?? '';
        if (!endpoint || !agentId) return null;

        const theme: ChatWidgetTheme | undefined = this.overrides.theme;
        const modeAttr = this.getAttribute('mode');
        const mode: ChatWidgetMode = this.overrides.mode ?? (modeAttr === 'fullpage' ? 'fullpage' : modeAttr === 'popover' ? 'popover' : undefined) ?? 'popover';
        return {
            endpoint,
            mode,
            agentId,
            agentName: this.overrides.agentName ?? this.getAttribute('agent-name') ?? undefined,
            userName: this.overrides.userName,
            userEmail: this.overrides.userEmail,
            placeholder: this.overrides.placeholder ?? this.getAttribute('placeholder') ?? undefined,
            greeting: this.overrides.greeting ?? this.getAttribute('greeting') ?? undefined,
            connectionErrorMessage: this.overrides.connectionErrorMessage,
            startOpen: this.overrides.startOpen ?? this.hasAttribute('start-open'),
            theme,
        };
    }

    // ───────────────────────────────── Render ──────────────────────────────────

    private render(): void {
        const config = this.readConfig();
        if (!config) {
            this.root.innerHTML = '';
            return;
        }
        const resolved = resolveConfig(config);

        // (Re)create the controller only when there isn't one yet. Attribute churn
        // (e.g. theme tweaks) re-renders the view without dropping the session.
        if (!this.controller) {
            this.controller = new ConversationController(config, {
                onMessages: (messages) => {
                    this.messages = messages;
                    this.renderMessages(resolved.greeting);
                },
                onStatus: (status) => {
                    this.status = status;
                    this.renderStatus();
                    this.renderComposerState();
                },
            });
            if (resolved.startOpen) this.open = true;
        }

        const fullpage = resolved.mode === 'fullpage';
        // Full-page mode is always "open" — it fills its container and has no
        // launcher to toggle.
        if (fullpage) this.open = true;

        const style = document.createElement('style');
        style.textContent = buildStyles(resolved.theme, resolved.mode);

        // Header: in full-page mode lead with the Smooth logo (falls back to the
        // agent name) + a subtle "powered by smooth-operator"; in popover mode the
        // compact agent-name title we've always shown. The close button only
        // exists in popover mode (full-page has nothing to collapse to).
        const headerBrand = fullpage
            ? `<div class="brand">
                    <span class="logo-wrap">${SMOOTH_LOGO_SVG}</span>
                    <div>
                        <div class="title">${escapeHtml(resolved.agentName)}</div>
                        <div class="status"></div>
                    </div>
                </div>
                <div class="powered">powered by smooth-operator</div>`
            : `<div class="brand">
                    <div>
                        <div class="title">${escapeHtml(resolved.agentName)}</div>
                        <div class="status"></div>
                    </div>
                </div>
                <button class="close" aria-label="Close chat">×</button>`;

        const container = document.createElement('div');
        container.innerHTML = `
            ${fullpage ? '' : '<button class="launcher" part="launcher" aria-label="Open chat">💬</button>'}
            <div class="panel${fullpage ? ' fullpage' : ' hidden'}" part="panel" role="${fullpage ? 'region' : 'dialog'}" aria-label="${escapeHtml(resolved.agentName)} chat">
                <div class="header">
                    ${headerBrand}
                </div>
                <div class="messages"></div>
                <div class="composer">
                    <textarea rows="1" placeholder="${escapeHtml(resolved.placeholder)}"></textarea>
                    <button class="send" type="button">Send</button>
                </div>
            </div>
        `;

        // Tag the logo <svg> so styles can size it (the inlined SVG has its own id).
        const logoSvg = container.querySelector('.logo-wrap svg');
        if (logoSvg) logoSvg.setAttribute('class', 'logo');

        this.root.replaceChildren(style, container);

        this.launcherEl = container.querySelector('.launcher');
        this.panelEl = container.querySelector('.panel');
        this.messagesEl = container.querySelector('.messages');
        this.statusEl = container.querySelector('.status');
        this.inputEl = container.querySelector('textarea');
        this.sendBtn = container.querySelector('.send');

        this.launcherEl?.addEventListener('click', () => this.openChat());
        container.querySelector('.close')?.addEventListener('click', () => this.closeChat());
        this.sendBtn?.addEventListener('click', () => this.submit());
        this.inputEl?.addEventListener('keydown', (ev) => {
            if (ev.key === 'Enter' && !ev.shiftKey) {
                ev.preventDefault();
                this.submit();
            }
        });

        // Full-page mode connects eagerly (there's no launcher click to trigger it).
        if (fullpage) void this.controller?.connect().catch(() => {});

        this.syncOpenState();
        this.renderMessages(resolved.greeting);
        this.renderStatus();
        this.renderComposerState();
    }

    private syncOpenState(): void {
        // In full-page mode the panel always fills the host; nothing to toggle.
        if (this.panelEl?.classList.contains('fullpage')) {
            this.inputEl?.focus();
            return;
        }
        this.panelEl?.classList.toggle('hidden', !this.open);
        this.launcherEl?.classList.toggle('hidden', this.open);
        if (this.open) this.inputEl?.focus();
    }

    private renderMessages(greeting: string): void {
        if (!this.messagesEl) return;
        this.messagesEl.replaceChildren();

        if (this.messages.length === 0 && greeting) {
            const g = document.createElement('div');
            g.className = 'bubble assistant greeting';
            g.textContent = greeting;
            this.messagesEl.appendChild(g);
        }

        for (const msg of this.messages) {
            const el = document.createElement('div');
            el.className = `bubble ${msg.role}`;
            if (msg.streaming && !msg.text) {
                el.classList.add('cursor');
            } else if (msg.streaming) {
                el.classList.add('cursor');
                el.textContent = msg.text;
            } else {
                el.textContent = msg.text;
            }
            this.messagesEl.appendChild(el);

            // Render a "Sources (N)" section under any assistant message whose
            // terminal eventual_response carried citations. Back-compatible: most
            // turns have none, so this is skipped.
            if (msg.role === 'assistant' && !msg.streaming && msg.citations && msg.citations.length > 0) {
                this.messagesEl.appendChild(this.renderSources(msg.citations));
            }
        }
        this.messagesEl.scrollTop = this.messagesEl.scrollHeight;
    }

    /**
     * Build the collapsible "Sources (N)" block for an assistant message's
     * citations. Each source renders its `title` (linked to `citation.url` when
     * present — `target=_blank rel=noopener` — plain text otherwise) plus the
     * grounding `snippet`. Built with DOM APIs (not innerHTML) so citation text
     * can't inject markup.
     */
    private renderSources(citations: Citation[]): HTMLElement {
        const wrap = document.createElement('div');
        wrap.className = 'sources';
        wrap.setAttribute('part', 'sources');

        const details = document.createElement('details');
        details.open = true;

        const summary = document.createElement('summary');
        summary.textContent = `Sources (${citations.length})`;
        details.appendChild(summary);

        const list = document.createElement('ol');
        for (const c of citations) {
            const li = document.createElement('li');

            let titleEl: HTMLElement;
            // SECURITY: only absolute http(s) URLs may become a link href. A
            // citation URL comes from indexed content (web/GitHub connectors), so
            // an attacker-influenceable doc could carry `javascript:`/`data:`/
            // `vbscript:` — assigning those to `a.href` is a one-click XSS. Anything
            // that isn't a valid absolute http(s) URL renders as plain text.
            const safeUrl = safeHttpUrl(c.url);
            if (safeUrl) {
                const a = document.createElement('a');
                a.className = 'src-title';
                a.href = safeUrl;
                a.target = '_blank';
                a.rel = 'noopener noreferrer';
                titleEl = a;
            } else {
                titleEl = document.createElement('span');
                titleEl.className = 'src-title';
            }
            titleEl.textContent = c.title || c.id || 'Source';
            li.appendChild(titleEl);

            if (c.snippet) {
                const snip = document.createElement('span');
                snip.className = 'src-snippet';
                snip.textContent = c.snippet;
                li.appendChild(snip);
            }
            list.appendChild(li);
        }
        details.appendChild(list);
        wrap.appendChild(details);
        return wrap;
    }

    private renderStatus(): void {
        if (!this.statusEl) return;
        const label: Record<ConnectionStatus, string> = {
            idle: '',
            connecting: 'Connecting…',
            ready: 'Online',
            error: 'Connection issue',
            closed: 'Disconnected',
        };
        this.statusEl.textContent = label[this.status];
    }

    private renderComposerState(): void {
        const busy = this.status === 'connecting';
        if (this.sendBtn) this.sendBtn.disabled = busy;
        if (this.inputEl) this.inputEl.disabled = busy;
    }

    private submit(): void {
        if (!this.inputEl || !this.controller) return;
        const text = this.inputEl.value;
        if (!text.trim()) return;
        this.inputEl.value = '';
        void this.controller.send(text);
    }
}

function escapeHtml(value: string): string {
    return value.replace(/[&<>"']/g, (c) => {
        switch (c) {
            case '&':
                return '&amp;';
            case '<':
                return '&lt;';
            case '>':
                return '&gt;';
            case '"':
                return '&quot;';
            default:
                return '&#39;';
        }
    });
}

/** Register the custom element once. Safe to call multiple times. */
export function defineChatWidget(): void {
    if (typeof customElements !== 'undefined' && !customElements.get(ELEMENT_TAG)) {
        customElements.define(ELEMENT_TAG, SmoothAgentChatElement);
    }
}

/**
 * Programmatically create, configure, and append a widget to the page.
 * Returns the element so the host can drive `openChat()` / `closeChat()`.
 */
export function mountChatWidget(config: ChatWidgetConfig, target: HTMLElement = document.body): SmoothAgentChatElement {
    defineChatWidget();
    const el = document.createElement(ELEMENT_TAG) as SmoothAgentChatElement;
    el.configure(config);
    target.appendChild(el);
    return el;
}

/**
 * Ergonomic helper for the full-page layout: mounts a `<smooth-agent-chat>` in
 * `mode: "fullpage"` (no launcher — the chat fills its container/viewport with a
 * Smooth-branded header, a scrollable message list, and an input bar) and
 * returns the element.
 *
 * `target` defaults to `document.body`; pass a sized container to embed the
 * full-page chat inside a layout region (e.g. a `/chat` route shell or an
 * iframe). The `mode` is forced to `"fullpage"` regardless of the passed config.
 *
 * ```ts
 * mountFullPageChat({ endpoint: 'wss://…/ws', agentId: '…', agentName: 'Support' });
 * ```
 */
export function mountFullPageChat(config: Omit<ChatWidgetConfig, 'mode'>, target: HTMLElement = document.body): SmoothAgentChatElement {
    return mountChatWidget({ ...config, mode: 'fullpage' }, target);
}
