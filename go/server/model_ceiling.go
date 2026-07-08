package server

import "encoding/json"

// Model-output ceiling clamp + raised turn defaults (EPIC th-1cc9fa).
//
// A budget max_tokens can exceed what a model can physically emit — reasoning
// models then burn the budget on reasoning_content and return EMPTY, or the
// upstream 400s (e.g. groq-compound caps output at 8192). The runner clamps every
// turn's max_tokens to min(DefaultMaxTokens, model ceiling), where the ceiling is
// sourced from the LiteLLM gateway's /model/info (model_info.max_output_tokens).
//
// Go parity with the Rust engine's LlmClient::effective_max_tokens and the Rust
// server's config defaults + /model/info mapping. The Go server pins a published
// core, so the clamp lives here (not via the engine's not-yet-published
// AgentOptions.ModelMaxOutput) — same runtime effect: the request carries an
// already-clamped max_tokens.

const (
	// DefaultMaxTokens is the per-turn max_tokens the server sends. Raised from the
	// old chat-widget-sized 512 (which starves reasoning models) — safe now that the
	// per-model ceiling caps runaway output; concise answers stay concise.
	DefaultMaxTokens = 8192
	// DefaultMaxIterations is the per-turn agent-loop cap. Raised from 6 for the same
	// reason — a reasoning model needs room to think, act, and answer.
	DefaultMaxIterations = 20
)

// clampMaxTokens returns min(configured, *ceiling): the configured budget clamped
// down to the model's hard output ceiling when one is known. A nil or non-positive
// ceiling leaves the budget unclamped. The result is never 0 for a positive budget
// (some gateways reject max_tokens=0). Mirror of the engine's effectiveMaxTokens.
func clampMaxTokens(configured int, ceiling *int) int {
	if ceiling == nil || *ceiling <= 0 || *ceiling >= configured {
		return configured
	}
	return *ceiling
}

// modelInfoResponse is the LiteLLM gateway's /model/info payload shape (only the
// fields we read): { data: [ { model_name, model_info: { max_output_tokens } } ] }.
type modelInfoResponse struct {
	Data []struct {
		ModelName string `json:"model_name"`
		ModelInfo struct {
			MaxOutputTokens *int `json:"max_output_tokens"`
		} `json:"model_info"`
	} `json:"data"`
}

// modelOutputCeiling extracts the hard output ceiling (model_info.max_output_tokens)
// for model from a gateway /model/info response body, for feeding WithModelCeiling.
// Returns nil when the body is unparseable, the model is absent, or its ceiling is
// missing/non-positive — every "unknown" case leaves the turn unclamped (graceful,
// best-effort, no behaviour change). Pure + network-free so it's unit-testable on a
// sample payload. Mirror of the Rust server's map_model_info max_output extraction.
func modelOutputCeiling(payload []byte, model string) *int {
	var resp modelInfoResponse
	if err := json.Unmarshal(payload, &resp); err != nil {
		return nil
	}
	for _, entry := range resp.Data {
		if entry.ModelName != model {
			continue
		}
		if c := entry.ModelInfo.MaxOutputTokens; c != nil && *c > 0 {
			return c
		}
		return nil
	}
	return nil
}
