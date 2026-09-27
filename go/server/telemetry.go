package server

import (
	"context"
	"encoding/json"
	"os"
	"sort"
	"strings"
	"sync"

	"go.opentelemetry.io/otel"
	"go.opentelemetry.io/otel/attribute"
	"go.opentelemetry.io/otel/exporters/otlp/otlptrace/otlptracehttp"
	sdkresource "go.opentelemetry.io/otel/sdk/resource"
	sdktrace "go.opentelemetry.io/otel/sdk/trace"
)

// OpenTelemetry GenAI instrumentation for the agent turn — the Go sibling of the Rust
// server's `telemetry.rs`. The turn runner opens a `gen_ai.chat` span per turn and a
// `gen_ai.tool` child span per tool call, carrying the GenAI semantic-convention
// attributes below, so the traces this host emits interoperate with the Rust host's and
// the smooai monorepo's existing `gen_ai.*` spans.

// GenAI semantic-convention attribute keys. The exact strings the Rust telemetry.rs uses,
// kept as named constants so the two hosts and any downstream consumer agree.
const (
	// GenAISystem is `gen_ai.system` — the GenAI system / provider name.
	GenAISystem = "gen_ai.system"
	// GenAIRequestModel is `gen_ai.request.model` — the model requested for the turn.
	GenAIRequestModel = "gen_ai.request.model"
	// GenAIConversationID is `gen_ai.conversation.id` — the conversation this turn belongs to.
	GenAIConversationID = "gen_ai.conversation.id"
	// GenAIUsageInputTokens is `gen_ai.usage.input_tokens` — prompt tokens consumed.
	GenAIUsageInputTokens = "gen_ai.usage.input_tokens"
	// GenAIUsageOutputTokens is `gen_ai.usage.output_tokens` — completion tokens produced.
	GenAIUsageOutputTokens = "gen_ai.usage.output_tokens"
	// GenAIToolName is `gen_ai.tool.name` — the name of an invoked tool.
	GenAIToolName = "gen_ai.tool.name"
	// GenAIToolArguments is `gen_ai.tool.call.arguments` — the JSON tool args. Kept for
	// API compatibility, but the tool span NO LONGER records it (SMOODEV-3364): argument
	// values are customer PII — names, emails, phones, addresses in CRM writes — and a
	// secret-NAME denylist cannot catch them. See GenAIToolArgumentKeys.
	GenAIToolArguments = "gen_ai.tool.call.arguments"
	// GenAIToolArgumentKeys is `gen_ai.tool.argument_keys` — the sorted top-level argument
	// KEY NAMES of a tool call (see toolArgumentKeys). What the tool span records instead of
	// the values: "did the update include `phone`?" stays answerable; the number is not stored.
	GenAIToolArgumentKeys = "gen_ai.tool.argument_keys"
	// GenAIAgentName is `gen_ai.agent.name` — the agent/persona driving the turn.
	GenAIAgentName = "gen_ai.agent.name"
	// SmooaiOrgID is `smooai.org_id` — the owning org. Matches the monorepo TS chat
	// handler's attribute exactly so the observability studio groups Rust + Go turns by org.
	SmooaiOrgID = "smooai.org_id"
	// GenAIOperationName is `gen_ai.operation.name` — the operation a span represents.
	//
	// The api-prime OTLP ingest takes this attribute VERBATIM when present and only
	// derives it from the span name as a fallback, and its queries filter on
	// `operation_name = 'tool'`. So the values must be exactly OperationChat /
	// OperationTool — a spelling like "execute_tool" would land in the column and
	// match nothing.
	GenAIOperationName = "gen_ai.operation.name"
	// GenAIUsageCostUSD is `gen_ai.usage.cost_usd` — the turn's cost in USD.
	//
	// Recorded ONLY when positive. A zero is ambiguous: the gateway answers 0 for a
	// model it has no price for, and local pricing returns the free tier for anything
	// it does not recognise, so a zero means "not measured", never "free". Exporting
	// it would render a paid turn as a confident $0.00.
	GenAIUsageCostUSD = "gen_ai.usage.cost_usd"
	// CostUnavailable is `smooai.gen_ai.cost_unavailable` — why GenAIUsageCostUSD is
	// absent. Set INSTEAD of the cost, never alongside it. Same attribute name and
	// values across every engine so a consumer never special-cases per language.
	CostUnavailable = "smooai.gen_ai.cost_unavailable"
	// CostUnavailableUnpriced is the CostUnavailable value for "no price could be
	// established for this model".
	CostUnavailableUnpriced = "unpriced"
)

// OperationChat / OperationTool are the GenAIOperationName values.
const (
	OperationChat = "chat"
	OperationTool = "tool"
)

// SystemName is emitted for GenAISystem and used as the tracer + service name.
const SystemName = "smooth-operator"

// AgentName is emitted as GenAIAgentName on the turn span — the same agent name the Rust
// reference runner builds its AgentConfig with.
const AgentName = "smooth-agent-chat"

// SpanChat / SpanTool are the span names, matching the Rust reference.
const (
	SpanChat = "gen_ai.chat"
	SpanTool = "gen_ai.tool"
)

// DefaultModel is the main-turn model the server requests when the turn has no explicit
// model set — the Smoo AI gateway's standard chat tier, in lockstep with the Rust
// reference's DEFAULT_MODEL (SMOODEV-3342). The turn runner passes it to the engine
// explicitly (AgentOptions.Model) rather than leaving the model empty, so the engine's own
// built-in fallback model is never what goes on the wire. It is also what the turn's OTel
// span records as gen_ai.request.model.
const DefaultModel = "gpt-6-luna"

// otlpEndpointEnv, when set, switches InitTelemetry from the local-only no-op provider to
// a real OTLP exporter. Matches the Rust server's OTLP_ENDPOINT_ENV gate.
const otlpEndpointEnv = "OTEL_EXPORTER_OTLP_ENDPOINT"

var initTelemetryOnce sync.Once

// InitTelemetry installs an OTLP (HTTP) span exporter when OTEL_EXPORTER_OTLP_ENDPOINT is
// set; when it is unset, the global no-op tracer provider stays in place, so the binary
// and tests run with zero external dependencies (spans become cheap no-ops). Mirrors the
// Rust server's env-gated `init_telemetry`. Idempotent — safe to call once at startup.
//
// Returns a shutdown func that flushes the batch exporter (a no-op when no exporter was
// installed); call it on process exit so buffered spans are not lost.
func InitTelemetry(ctx context.Context) (shutdown func(context.Context) error, err error) {
	shutdown = func(context.Context) error { return nil }
	if strings.TrimSpace(os.Getenv(otlpEndpointEnv)) == "" {
		// No endpoint configured — local-only, no exporter. No collector needed.
		return shutdown, nil
	}
	initTelemetryOnce.Do(func() {
		// otlptracehttp.New reads the endpoint (and other OTEL_EXPORTER_OTLP_* knobs)
		// from the environment, matching the Rust exporter's WithExportConfig.
		exp, e := otlptracehttp.New(ctx)
		if e != nil {
			// A bad endpoint must never take down the host: fall back to the no-op
			// provider, exactly like the Rust server's build_otlp_layer error arm.
			err = e
			return
		}
		tp := sdktrace.NewTracerProvider(
			sdktrace.WithBatcher(exp),
			sdktrace.WithResource(sdkresource.NewSchemaless(
				attribute.String("service.name", SystemName),
			)),
		)
		otel.SetTracerProvider(tp)
		shutdown = tp.Shutdown
	})
	return shutdown, err
}

// toolArgumentKeys returns the sorted top-level key names of a tool's serialized JSON
// arguments, comma-joined — a Go port of the Rust telemetry.rs `tool_argument_keys`. It
// NEVER returns a value: a JSON object yields its keys; `null` or empty input yields "";
// an array "<array>"; any other scalar "<scalar>"; unparseable input "<unparsed>".
func toolArgumentKeys(arguments string) string {
	if strings.TrimSpace(arguments) == "" {
		return ""
	}
	var value any
	if err := json.Unmarshal([]byte(arguments), &value); err != nil {
		return "<unparsed>"
	}
	switch v := value.(type) {
	case nil:
		return ""
	case map[string]any:
		keys := make([]string, 0, len(v))
		for k := range v {
			keys = append(keys, k)
		}
		sort.Strings(keys)
		return strings.Join(keys, ",")
	case []any:
		return "<array>"
	default:
		return "<scalar>"
	}
}
