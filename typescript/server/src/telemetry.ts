/**
 * OpenTelemetry GenAI spans for the TypeScript server — the parity of
 * `rust/smooth-operator/src/telemetry.rs` + `smooth-operator-server`'s span points.
 *
 * A turn opens a {@link SPAN_CHAT} (`gen_ai.chat`) span carrying `gen_ai.system`,
 * `gen_ai.request.model`, `gen_ai.conversation.id`, `gen_ai.agent.name`, and
 * `smooai.org_id`; token usage is recorded onto it on completion. Each tool call the
 * engine emits opens a child {@link SPAN_TOOL} (`gen_ai.tool`) span with the tool name
 * and its redacted arguments — matching the Rust reference attribute names so the
 * studio groups the polyglot servers identically.
 *
 * {@link initTelemetry} is env-gated exactly like the Rust `init_telemetry`: it wires a
 * real OTLP exporter only when `OTEL_EXPORTER_OTLP_ENDPOINT` is set. Unset ⇒ no
 * exporter is registered, so `trace.getTracer` yields no-op spans (the collector-less
 * binary + test path). Tests register their own in-memory provider instead.
 */
import { context, trace, type Span, type Tracer } from '@opentelemetry/api';

// --- GenAI attribute keys (OTel semantic conventions), matching telemetry.rs. ---
export const GEN_AI_SYSTEM = 'gen_ai.system';
export const GEN_AI_REQUEST_MODEL = 'gen_ai.request.model';
export const GEN_AI_CONVERSATION_ID = 'gen_ai.conversation.id';
export const GEN_AI_USAGE_INPUT_TOKENS = 'gen_ai.usage.input_tokens';
export const GEN_AI_USAGE_OUTPUT_TOKENS = 'gen_ai.usage.output_tokens';
export const GEN_AI_TOOL_NAME = 'gen_ai.tool.name';
export const GEN_AI_TOOL_ARGUMENTS = 'gen_ai.tool.call.arguments';
export const GEN_AI_AGENT_NAME = 'gen_ai.agent.name';
export const SMOOAI_ORG_ID = 'smooai.org_id';
/**
 * `gen_ai.operation.name` — the operation a span represents.
 *
 * The api-prime OTLP ingest takes this attribute VERBATIM when present and only
 * derives it from the span name as a fallback, and its queries filter on
 * `operation_name = 'tool'`. So the values must be exactly {@link OPERATION_CHAT}
 * / {@link OPERATION_TOOL} — a spelling like `execute_tool` would land in the
 * column and match nothing.
 */
export const GEN_AI_OPERATION_NAME = 'gen_ai.operation.name';
/**
 * `gen_ai.usage.cost_usd` — the turn's cost in USD.
 *
 * Recorded ONLY when positive. A zero is ambiguous: the gateway answers `0` for
 * a model it has no price for, and local pricing returns the free tier for
 * anything it doesn't recognise, so a zero means "not measured", never "free".
 * Exporting it would render a paid turn as a confident $0.00.
 */
export const GEN_AI_USAGE_COST_USD = 'gen_ai.usage.cost_usd';
/**
 * `smooai.gen_ai.cost_unavailable` — why {@link GEN_AI_USAGE_COST_USD} is absent.
 * Set INSTEAD of the cost, never alongside it. Same attribute name and values
 * across every engine so a consumer never special-cases per language.
 */
export const COST_UNAVAILABLE = 'smooai.gen_ai.cost_unavailable';
/** {@link COST_UNAVAILABLE} value: no price could be established for the model. */
export const COST_UNAVAILABLE_UNPRICED = 'unpriced';

/** `gen_ai.system` value identifying the polyglot operator to the studio. */
export const SYSTEM_NAME = 'smooth-operator';
/** `gen_ai.agent.name` value — the chat agent, matching the Rust `AGENT_NAME`. */
export const AGENT_NAME = 'smooth-agent-chat';

/** Span name for the per-turn GenAI chat span (`gen_ai.chat`). */
export const SPAN_CHAT = 'gen_ai.chat';
/** Span name for a per-tool-call child span (`gen_ai.tool`). */
export const SPAN_TOOL = 'gen_ai.tool';

/** {@link GEN_AI_OPERATION_NAME} value on a {@link SPAN_CHAT} span. */
export const OPERATION_CHAT = 'chat';
/** {@link GEN_AI_OPERATION_NAME} value on a {@link SPAN_TOOL} span. */
export const OPERATION_TOOL = 'tool';

/** Instrumentation-scope name the tracer is fetched under. */
export const TRACER_NAME = SYSTEM_NAME;

/** Cap on recorded tool arguments, matching `telemetry.rs::MAX_TOOL_ARGS_LEN`. */
const MAX_TOOL_ARGS_LEN = 2048;

/** Env var that switches {@link initTelemetry} from no-op to a real OTLP exporter. */
export const OTLP_ENDPOINT_ENV = 'OTEL_EXPORTER_OTLP_ENDPOINT';

const SECRET_KEY_NEEDLES = [
    'secret',
    'token',
    'password',
    'api_key',
    'apikey',
    'authorization',
    'bearer',
    'credential',
    'access_key',
    'private_key',
];

function isSecretKey(key: string): boolean {
    const lower = key.toLowerCase();
    return SECRET_KEY_NEEDLES.some((n) => lower.includes(n));
}

function redactInPlace(value: unknown): unknown {
    if (Array.isArray(value)) return value.map(redactInPlace);
    if (value && typeof value === 'object') {
        const out: Record<string, unknown> = {};
        for (const [k, v] of Object.entries(value as Record<string, unknown>)) {
            out[k] = isSecretKey(k) ? '[REDACTED]' : redactInPlace(v);
        }
        return out;
    }
    return value;
}

function truncate(s: string, max: number): string {
    return s.length <= max ? s : `${s.slice(0, max)}…`;
}

/**
 * Redact a tool's serialized JSON arguments for span recording — the TS parity of
 * `telemetry.rs::redact_tool_arguments`. Replaces the value of any object key whose
 * name looks secret-bearing with `"[REDACTED]"`, passes non-JSON through as-is, and
 * always length-caps the result. Best-effort scrub keyed on argument *names*.
 */
export function redactToolArguments(argumentsJson: string): string {
    let out: string;
    try {
        out = JSON.stringify(redactInPlace(JSON.parse(argumentsJson)));
    } catch {
        out = argumentsJson;
    }
    return truncate(out, MAX_TOOL_ARGS_LEN);
}

/** The tracer the turn/tool spans are emitted under. */
export function getTracer(): Tracer {
    return trace.getTracer(TRACER_NAME);
}

/**
 * Open the per-turn `gen_ai.chat` span with the GenAI + org attributes.
 * `orgId` is recorded only when present, matching the Rust runner.
 */
export function startTurnSpan(model: string, conversationId: string, orgId: string | undefined): Span {
    const attributes: Record<string, string> = {
        [GEN_AI_SYSTEM]: SYSTEM_NAME,
        [GEN_AI_OPERATION_NAME]: OPERATION_CHAT,
        [GEN_AI_REQUEST_MODEL]: model,
        [GEN_AI_CONVERSATION_ID]: conversationId,
        [GEN_AI_AGENT_NAME]: AGENT_NAME,
    };
    if (orgId) attributes[SMOOAI_ORG_ID] = orgId;
    return getTracer().startSpan(SPAN_CHAT, { attributes });
}

/**
 * Record the turn's token counts and cost on the turn span — only the parts that
 * were actually measured.
 *
 * Counts are omitted entirely when the engine reported none: absent is honest,
 * `0` is a lie (a grounded turn always consumes prompt tokens). Cost is judged
 * separately, because the gateway reports it on an HTTP header while usage comes
 * on an SSE chunk — either can arrive without the other. A non-positive cost
 * becomes {@link COST_UNAVAILABLE} rather than a `$0.00`.
 */
export function recordTurnUsage(turnSpan: Span, usage: { promptTokens: number; completionTokens: number; costUsd: number } | undefined): void {
    if (!usage) return;
    if (usage.promptTokens > 0 || usage.completionTokens > 0) {
        turnSpan.setAttribute(GEN_AI_USAGE_INPUT_TOKENS, usage.promptTokens);
        turnSpan.setAttribute(GEN_AI_USAGE_OUTPUT_TOKENS, usage.completionTokens);
    }
    if (usage.costUsd > 0 && Number.isFinite(usage.costUsd)) {
        turnSpan.setAttribute(GEN_AI_USAGE_COST_USD, usage.costUsd);
    } else {
        turnSpan.setAttribute(COST_UNAVAILABLE, COST_UNAVAILABLE_UNPRICED);
    }
}

/**
 * Emit a child `gen_ai.tool` span for one tool call under `turnSpan`, carrying the
 * tool name and redacted arguments. `durationMs`, when known, is recorded too.
 */
export function recordToolSpan(
    turnSpan: Span,
    toolName: string,
    argumentsJson: string,
    conversationId: string,
    orgId?: string,
    durationMs?: number,
): void {
    const ctx = trace.setSpan(context.active(), turnSpan);
    // The OTLP ingest builds a span's attributes from the resource attrs plus
    // THAT span's own, with no inheritance from the parent — so a child repeats
    // its identifiers or it cannot be joined. Omitting `gen_ai.system` is worse
    // than losing the join: the ingest's LLM-event gate keys on it, so bare tool
    // spans are DISCARDED. Rust's were, for their entire existence.
    const attributes: Record<string, string | number> = {
        [GEN_AI_SYSTEM]: SYSTEM_NAME,
        [GEN_AI_OPERATION_NAME]: OPERATION_TOOL,
        [GEN_AI_CONVERSATION_ID]: conversationId,
        [GEN_AI_TOOL_NAME]: toolName,
        [GEN_AI_TOOL_ARGUMENTS]: redactToolArguments(argumentsJson),
    };
    if (orgId) attributes[SMOOAI_ORG_ID] = orgId;
    if (durationMs !== undefined) attributes.duration_ms = durationMs;
    getTracer().startSpan(SPAN_TOOL, { attributes }, ctx).end();
}

let initialized = false;

/**
 * Initialize OpenTelemetry tracing for the process — the parity of the Rust
 * `init_telemetry`. Idempotent.
 *
 * - `OTEL_EXPORTER_OTLP_ENDPOINT` set ⇒ register a tracer provider with an OTLP
 *   (HTTP/protobuf) batch exporter pointed at it.
 * - unset ⇒ do nothing; `trace.getTracer` returns no-op spans (no collector needed).
 *
 * A bad endpoint / exporter build must never take down the server, so failures fall
 * back to the no-op path with a warning rather than throwing.
 */
export async function initTelemetry(): Promise<boolean> {
    if (initialized) return false;
    const endpoint = process.env[OTLP_ENDPOINT_ENV]?.trim();
    if (!endpoint) return false;
    initialized = true;

    try {
        const [{ BasicTracerProvider, BatchSpanProcessor }, { OTLPTraceExporter }, { resourceFromAttributes }] = await Promise.all([
            import('@opentelemetry/sdk-trace-base'),
            import('@opentelemetry/exporter-trace-otlp-proto'),
            import('@opentelemetry/resources'),
        ]);
        const provider = new BasicTracerProvider({
            resource: resourceFromAttributes({ 'service.name': SYSTEM_NAME }),
            spanProcessors: [new BatchSpanProcessor(new OTLPTraceExporter({ url: endpoint }))],
        });
        trace.setGlobalTracerProvider(provider);
        console.error(`[telemetry] OTLP exporter installed → ${endpoint}`);
        return true;
    } catch (err) {
        console.error('[telemetry] OTLP exporter init failed; tracing disabled:', err);
        return false;
    }
}
