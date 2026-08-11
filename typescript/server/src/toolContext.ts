/**
 * Per-turn tool-provider context + the file-transfer plumbing (contract PR #342).
 *
 * The TypeScript analog of the Rust engine's `smooth_operator::tool_provider`
 * ({@link https}: `rust/smooth-operator/src/tool_provider.rs`). A `send_message`
 * frame may carry two kinds of attachment, handled asymmetrically to match Rust:
 *
 * - `images[]` — multimodal vision input. Attached to the turn's user message as
 *   OpenAI `image_url` content parts so the MODEL sees them (see
 *   {@link withUserImages}). The plain text is still what drives retrieval/memory.
 * - `files[]` — non-image bytes. NOT sent to the model; surfaced on the per-turn
 *   {@link ToolContext} so a host tool can persist them into the agent's workspace,
 *   where ordinary tools (read_file, bash, …) can then read them.
 *
 * The context also carries a **directive sink**: a host tool sets {@link
 * ToolContext.directive} to a client-side directive object (e.g. the `send_file`
 * agent→user delivery directive), which the dispatcher drains after the turn onto
 * `eventual_response.directive` (last-write-wins). Mirrors the Rust
 * `ToolProviderContext.directive_sink` → runner drain → `protocol` emit.
 *
 * The seam is optional and additive: with no {@link ToolProvider} installed and no
 * attachments, behaviour is byte-for-byte unchanged.
 */
import type { ChatChunk, ChatClientLike, Tool } from '@smooai/smooth-operator-core';

/** OpenAI vision detail hint. */
export type ImageDetail = 'low' | 'high' | 'auto';

/**
 * An image attachment on a multimodal turn's user message. Mirrors the wire
 * `send_message.images[]` item and the Rust `UserImage`. `url` is a `data:` image
 * URL or a remote `https` URL; `detail` is the optional OpenAI vision hint.
 */
export interface UserImage {
    url: string;
    detail?: ImageDetail;
}

/**
 * A non-image file attachment on a turn. Mirrors the wire `send_message.files[]`
 * item. Surfaced to host tools via {@link ToolContext.files}; never sent to the
 * model. `name` is a basename hint (the host sanitizes + confines to the
 * workspace); `url` is a `data:<mime>;base64,...` (or `https`) URL carrying the
 * bytes.
 */
export interface UserFile {
    name: string;
    url: string;
    mimeType?: string;
}

/**
 * The per-turn context a {@link ToolProvider} sees. Mirrors the Rust
 * `ToolProviderContext`: the attachments the turn carried, plus a mutable
 * directive sink a host tool writes for this turn.
 */
export interface ToolContext {
    /** The turn's image attachments (also attached to the model via {@link withUserImages}). */
    images: UserImage[];
    /** The turn's non-image file attachments — for host tools, never sent to the model. */
    files: UserFile[];
    /**
     * Directive sink (last-write-wins). A host tool sets this to a client-side
     * directive object (opaque to the protocol layer, like `response`); the
     * dispatcher drains it after the turn onto `eventual_response.directive`.
     * `undefined` ⇒ the turn produced no directive and the field is omitted.
     */
    directive?: unknown;
}

/**
 * A per-turn host seam: given the turn's {@link ToolContext}, return the extra
 * tools to merge into this turn's registry. The returned tools may close over
 * `ctx` — reading {@link ToolContext.files} / {@link ToolContext.images} and
 * writing {@link ToolContext.directive}. Mirrors the Rust
 * `ToolProvider::tools_for`. Returning `[]` (or not installing a provider) leaves
 * the registry as exactly the static tools.
 */
export type ToolProvider = (ctx: ToolContext) => Tool[] | Promise<Tool[]>;

/**
 * Parse the wire `images[]` fail-soft: drop any entry without a non-empty string
 * `url`, and keep `detail` only when it is a valid vision hint. Absent/non-array ⇒
 * `[]`. Never throws — a malformed entry is ignored rather than rejecting the turn
 * (per the schema).
 */
export function parseImages(raw: unknown): UserImage[] {
    if (!Array.isArray(raw)) return [];
    const out: UserImage[] = [];
    for (const item of raw) {
        if (item === null || typeof item !== 'object') continue;
        const rec = item as Record<string, unknown>;
        if (typeof rec.url !== 'string' || rec.url.length === 0) continue;
        const img: UserImage = { url: rec.url };
        if (rec.detail === 'low' || rec.detail === 'high' || rec.detail === 'auto') img.detail = rec.detail;
        out.push(img);
    }
    return out;
}

/**
 * Parse the wire `files[]` fail-soft: drop any entry missing a non-empty string
 * `name` or `url`; keep `mimeType` only when it is a string. Absent/non-array ⇒
 * `[]`. Never throws.
 */
export function parseFiles(raw: unknown): UserFile[] {
    if (!Array.isArray(raw)) return [];
    const out: UserFile[] = [];
    for (const item of raw) {
        if (item === null || typeof item !== 'object') continue;
        const rec = item as Record<string, unknown>;
        if (typeof rec.name !== 'string' || rec.name.length === 0) continue;
        if (typeof rec.url !== 'string' || rec.url.length === 0) continue;
        const file: UserFile = { name: rec.name, url: rec.url };
        if (typeof rec.mimeType === 'string') file.mimeType = rec.mimeType;
        out.push(file);
    }
    return out;
}

/**
 * Wrap a {@link ChatClientLike} so this turn's user message carries `images` as
 * OpenAI `image_url` content parts.
 *
 * The published core engine takes the user message as a plain string
 * (`runStream(message: string, …)`) and reuses that string for retrieval/memory,
 * so we cannot pass content parts through it directly. Instead we intercept the
 * outgoing request body and rewrite the CURRENT turn's user message content
 * (`"text"` → `[{type:'text',…}, {type:'image_url',…}]`) on the way to the
 * gateway — the exact shape the Rust engine's `with_user_images` produces — while
 * leaving the engine's own text-based retrieval untouched.
 *
 * The current turn's user message is the LAST `role:'user'` entry: across agent
 * iterations the engine appends only `assistant`/`tool` messages after it, so it
 * stays the last user message. A message whose content is already non-string is
 * left alone. Empty `images` ⇒ the client is returned unwrapped (zero overhead,
 * behaviour unchanged).
 *
 * ponytail: this is a request-body rewrite because the TS core lacks a multimodal
 * seam. Upgrade path: when `@smooai/smooth-operator-core` grows a `withUserImages`
 * / content-parts message API (as the Rust core has), attach there and delete this
 * wrapper.
 */
export function withUserImages(client: ChatClientLike, images: UserImage[]): ChatClientLike {
    if (images.length === 0) return client;

    const parts: Array<Record<string, unknown>> = images.map((img) => {
        const imageUrl: Record<string, unknown> = { url: img.url };
        if (img.detail !== undefined) imageUrl.detail = img.detail;
        return { type: 'image_url', image_url: imageUrl };
    });

    const patch = (body: Record<string, unknown>): Record<string, unknown> => {
        const messages = body.messages;
        if (!Array.isArray(messages)) return body;
        let idx = -1;
        for (let i = messages.length - 1; i >= 0; i--) {
            const m = messages[i] as Record<string, unknown> | undefined;
            if (m && m.role === 'user') {
                idx = i;
                break;
            }
        }
        if (idx === -1) return body;
        const target = messages[idx] as Record<string, unknown>;
        if (typeof target.content !== 'string') return body;
        const patched = messages.slice();
        patched[idx] = { ...target, content: [{ type: 'text', text: target.content }, ...parts] };
        return { ...body, messages: patched };
    };

    const underlyingStream = client.chat.completions.createStream;
    return {
        chat: {
            completions: {
                create: (body: Record<string, unknown>) => client.chat.completions.create(patch(body)),
                createStream: underlyingStream
                    ? (body: Record<string, unknown>): AsyncIterable<ChatChunk> => underlyingStream.call(client.chat.completions, patch(body))
                    : undefined,
            },
        },
    };
}
