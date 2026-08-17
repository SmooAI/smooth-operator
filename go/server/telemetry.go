package server

import (
	"context"
	"encoding/json"
	"os"
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
	// GenAIToolArguments is `gen_ai.tool.call.arguments` — the (redacted) JSON tool args.
	GenAIToolArguments = "gen_ai.tool.call.arguments"
	// GenAIAgentName is `gen_ai.agent.name` — the agent/persona driving the turn.
	GenAIAgentName = "gen_ai.agent.name"
	// SmooaiOrgID is `smooai.org_id` — the owning org. Matches the monorepo TS chat
	// handler's attribute exactly so the observability studio groups Rust + Go turns by org.
	SmooaiOrgID = "smooai.org_id"
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

// defaultTurnModel is recorded as gen_ai.request.model when the turn has no explicit
// model set. ponytail: mirrors the core engine's unexported `defaultModel`; the Go server
// leaves AgentOptions.Model empty today so the engine picks this. If per-agent model
// selection is wired into the turn runner, set TurnRunner.model and this falls away.
const defaultTurnModel = "claude-haiku-4-5"

// otlpEndpointEnv, when set, switches InitTelemetry from the local-only no-op provider to
// a real OTLP exporter. Matches the Rust server's OTLP_ENDPOINT_ENV gate.
const otlpEndpointEnv = "OTEL_EXPORTER_OTLP_ENDPOINT"

// maxToolArgsLen caps a serialized tool-arguments string recorded on a span so a
// pathological argument blob can't bloat span export. Matches the Rust MAX_TOOL_ARGS_LEN.
const maxToolArgsLen = 2048

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

// redactToolArguments redacts a tool's serialized JSON arguments for span recording,
// then length-caps the result. A Go port of the Rust telemetry.rs `redact_tool_arguments`:
// it walks parsed JSON and replaces the value of any object key whose name looks
// secret-bearing with "[REDACTED]". Non-JSON input is passed through as-is (still capped).
//
// This is a best-effort scrub keyed on argument NAMES, not a secret scanner — a secret
// passed under an innocuous key still lands.
func redactToolArguments(arguments string) string {
	var value any
	if err := json.Unmarshal([]byte(arguments), &value); err != nil {
		// Not JSON — record the raw string; still length-capped below.
		return truncateRunes(arguments, maxToolArgsLen)
	}
	redactJSONInPlace(&value)
	out, err := json.Marshal(value)
	if err != nil {
		return truncateRunes(arguments, maxToolArgsLen)
	}
	return truncateRunes(string(out), maxToolArgsLen)
}

// isSecretKey reports whether an object key name looks like it holds a secret value.
// The needle set matches the Rust telemetry.rs is_secret_key.
func isSecretKey(key string) bool {
	needles := []string{
		"secret", "token", "password", "api_key", "apikey",
		"authorization", "bearer", "credential", "access_key", "private_key",
	}
	lower := strings.ToLower(key)
	for _, n := range needles {
		if strings.Contains(lower, n) {
			return true
		}
	}
	return false
}

// redactJSONInPlace recursively replaces secret-named object values with "[REDACTED]".
func redactJSONInPlace(value *any) {
	switch v := (*value).(type) {
	case map[string]any:
		for k := range v {
			if isSecretKey(k) {
				v[k] = "[REDACTED]"
				continue
			}
			child := v[k]
			redactJSONInPlace(&child)
			v[k] = child
		}
	case []any:
		for i := range v {
			child := v[i]
			redactJSONInPlace(&child)
			v[i] = child
		}
	}
}

// truncateRunes caps s to at most max bytes on a rune boundary, appending "…" when cut.
// Mirrors the Rust telemetry.rs `truncate`.
func truncateRunes(s string, max int) string {
	if len(s) <= max {
		return s
	}
	end := max
	for end > 0 && !utf8RuneStart(s[end]) {
		end--
	}
	return s[:end] + "…"
}

// utf8RuneStart reports whether b is not a UTF-8 continuation byte (i.e. a rune boundary
// starts at it) — the analog of Rust's str::is_char_boundary for a byte index.
func utf8RuneStart(b byte) bool { return b&0xC0 != 0x80 }
