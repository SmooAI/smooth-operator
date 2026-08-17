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

/** `gen_ai.system` value identifying the polyglot operator to the studio. */
export const SYSTEM_NAME = 'smooth-operator';
/** `gen_ai.agent.name` value — the chat agent, matching the Rust `AGENT_NAME`. */
export const AGENT_NAME = 'smooth-agent-chat';

/** Span name for the per-turn GenAI chat span (`gen_ai.chat`). */
export const SPAN_CHAT = 'gen_ai.chat';
/** Span name for a per-tool-call child span (`gen_ai.tool`). */
export const SPAN_TOOL = 'gen_ai.tool';

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
        [GEN_AI_REQUEST_MODEL]: model,
        [GEN_AI_CONVERSATION_ID]: conversationId,
        [GEN_AI_AGENT_NAME]: AGENT_NAME,
    };
    if (orgId) attributes[SMOOAI_ORG_ID] = orgId;
    return getTracer().startSpan(SPAN_CHAT, { attributes });
}

/**
 * Emit a child `gen_ai.tool` span for one tool call under `turnSpan`, carrying the
 * tool name and redacted arguments. `durationMs`, when known, is recorded too.
 */
export function recordToolSpan(turnSpan: Span, toolName: string, argumentsJson: string, durationMs?: number): void {
    const ctx = trace.setSpan(context.active(), turnSpan);
    const attributes: Record<string, string | number> = {
        [GEN_AI_TOOL_NAME]: toolName,
        [GEN_AI_TOOL_ARGUMENTS]: redactToolArguments(argumentsJson),
    };
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
