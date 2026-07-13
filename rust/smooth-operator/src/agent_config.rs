//! Per-agent behavior config (SMOODEV-590 parity in Rust).
//!
//! A public chat agent served over `wss://ai.smoo.ai/ws` must behave as the
//! agent its owner configured — not as a generic customer-support bot. The
//! monorepo `agents` row carries the per-agent knobs:
//!
//! - `instructions.prompt` — the agent's persona / system prompt,
//! - `personality.persona` — an optional custom-persona addendum,
//! - `greeting` — an optional channel-agnostic opening line,
//! - `conversation_workflow` — an optional stepped, judge-advanced guided flow.
//!
//! The reference server resolves the turn's system prompt from **per-org**
//! settings (see [`crate::settings`]); that gives every agent in an org the same
//! voice and never applies `conversation_workflow`. This module is the
//! **per-agent** seam: a host installs an [`AgentConfigResolver`] (backed by the
//! `agents` table) so the runner can key behavior off the connection's
//! `agent_id`. Session-create carries only an agent UUID, so config is resolved
//! server-side by id (matching the sibling lanes' `AgentConfigResolver.resolve`).
//!
//! Everything here is I/O-free and jsonb-tolerant on purpose: a malformed row
//! degrades to "no per-agent config" (fall back to the org default) rather than
//! failing the turn. The resolver trait is the only async surface.
//!
//! Mirrors the TS reference in
//! `packages/backend/src/ai/graphs/general-agent/workflow.ts` +
//! `nodes/workflow-judge.ts`.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use smooth_operator_core::tool::{ToolCall, ToolHook};

/// One step of a structured conversation workflow. Mirrors
/// `ConversationWorkflowStep` (`packages/schemas/src/agents/agent.ts`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationWorkflowStep {
    /// Stable id, referenced by [`next`](Self::next) and the conversation's
    /// tracked pointer.
    pub id: String,
    /// What the agent should try to accomplish on this step.
    pub intent: String,
    /// Objective criteria the judge evaluates to decide whether the step was
    /// satisfied this turn.
    pub criteria: String,
    /// Step id to advance to once criteria are met. Omit / empty on terminal
    /// steps (advancement then falls through to the next array element).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
}

/// A structured conversation workflow: a goal + ordered steps. Mirrors
/// `ConversationWorkflow`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationWorkflow {
    /// Overall goal the agent drives toward across the conversation.
    pub goal: String,
    /// Ordered steps; the first is the starting point.
    pub steps: Vec<ConversationWorkflowStep>,
}

/// One entry in `tool_config.enabledTools` (the monorepo `AgentToolConfig`
/// shape). `auth_level` / `config` are preserved on the parsed type for
/// downstream hosts even though the reference server doesn't act on them yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnabledTool {
    /// The tool's snake_case id (e.g. `knowledge_search`).
    pub tool_id: String,
    /// Whether the tool is enabled for this agent.
    pub enabled: bool,
    /// Auth level the tool requires (`none` by default). Carried for hosts.
    pub auth_level: String,
    /// Opaque per-tool config. Carried for hosts.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub config: serde_json::Value,
}

/// One entry in `extension_config.enabledExtensions` (the monorepo per-agent SEP
/// enablement shape). `config` is preserved on the parsed type for downstream
/// hosts even though the reference server doesn't act on it yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnabledExtension {
    /// The extension's kebab-case id (e.g. `plan-mode`), matching the
    /// `extension.toml` manifest name.
    pub extension_id: String,
    /// Whether the extension is enabled for this agent.
    pub enabled: bool,
    /// Opaque per-extension config. Carried for hosts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<serde_json::Value>,
}

/// Auth level a tool requires (monorepo `AuthLevel`, `agent.ts`). Gating only
/// applies when this is not [`None`](AuthLevel::None) and the tool supports auth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthLevel {
    /// No authentication required (the default).
    #[default]
    None,
    /// The end user's identity must be verified (OTP on a public agent).
    EndUser,
    /// Admin authentication — only satisfiable on an internal agent.
    Admin,
}

impl AuthLevel {
    /// Parse from the `authLevel` string, defaulting to [`None`](Self::None).
    #[must_use]
    pub fn parse(s: &str) -> Self {
        match s {
            "end_user" => Self::EndUser,
            "admin" => Self::Admin,
            _ => Self::None,
        }
    }
}

/// Where an agent is reachable (monorepo `AgentVisibility`). `internal` agents
/// run behind an authenticated dashboard session, so their tool auth is
/// auto-satisfied; `public` agents (the default) are widget-embeddable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    /// Public widget-embeddable agent (the default).
    #[default]
    Public,
    /// Internal dashboard-only agent (authenticated session).
    Internal,
}

impl Visibility {
    /// Parse from the `visibility` string, defaulting to [`Public`](Self::Public).
    #[must_use]
    pub fn parse(s: &str) -> Self {
        match s {
            "internal" => Self::Internal,
            _ => Self::Public,
        }
    }
}

/// Decide whether a tool call is allowed given its required auth level, the
/// agent's visibility, and whether the session is identity-verified. Mirrors
/// `tool-execution.ts` (lines ~145-190). `None` (allow) or `Some(message)` (the
/// reference refusal the model is shown). Callers gate ONLY when `level !=
/// AuthLevel::None` AND the tool supports auth requirements.
///
/// - internal agent → auto-satisfied (both `end_user` and `admin`);
/// - public + `admin` → refuse (admin tools never run on public agents);
/// - public + `end_user` → satisfied only when the session is identity-verified,
///   else refuse asking for verification (the OTP flow is host wiring behind
///   this seam — here the default is fail-closed).
#[must_use]
pub fn tool_auth_refusal(
    tool_name: &str,
    level: AuthLevel,
    visibility: Visibility,
    session_authenticated: bool,
) -> Option<String> {
    if visibility == Visibility::Internal {
        return None; // authenticated dashboard session satisfies any level
    }
    match level {
        AuthLevel::None => None,
        AuthLevel::Admin => Some(format!(
            "Tool '{tool_name}' requires admin authentication and is not available on public-facing agents."
        )),
        AuthLevel::EndUser => {
            if session_authenticated {
                None
            } else {
                Some(format!(
                    "I need to verify your identity before I can use {tool_name}. Please verify with a one-time code."
                ))
            }
        }
    }
}

/// A [`ToolHook`] that blocks a tool call whose configured [`AuthLevel`] isn't
/// satisfied — the operator-side analog of `tool-execution.ts`'s auth gate. A
/// blocked call surfaces the reference refusal to the model (the engine turns a
/// `pre_call` error into the tool result), so the tool never executes.
///
/// Only tools in [`auth_supporting_tools`](Self::auth_supporting_tools) are gated
/// (the `supportsAuthRequirement` flag; empty ⇒ the hook is inert — every current
/// built-in). The identity-verified `session_authenticated` bit is the seam a
/// host with an OTP flow flips; the reference server leaves it fail-closed
/// (`false`).
#[derive(Debug, Clone)]
pub struct AuthGateHook {
    auth_levels: HashMap<String, AuthLevel>,
    visibility: Visibility,
    session_authenticated: bool,
    auth_supporting_tools: HashSet<String>,
    /// Captures the name of an `end_user` tool this hook refused because the
    /// session was not yet identity-verified — the one refusal an OTP flow can
    /// remedy (an `admin` refusal never can). The server reads it after the turn
    /// to decide whether to offer OTP. `Arc<Mutex<…>>` because the hook is cloned
    /// into the engine's tool path yet the server keeps a handle to observe it.
    otp_refused_tool: Arc<Mutex<Option<String>>>,
}

impl AuthGateHook {
    /// Build the gate from an agent's resolved auth levels + visibility. Only the
    /// tools in `auth_supporting_tools` are ever gated.
    #[must_use]
    pub fn new(
        auth_levels: HashMap<String, AuthLevel>,
        visibility: Visibility,
        session_authenticated: bool,
        auth_supporting_tools: HashSet<String>,
    ) -> Self {
        Self {
            auth_levels,
            visibility,
            session_authenticated,
            auth_supporting_tools,
            otp_refused_tool: Arc::new(Mutex::new(None)),
        }
    }

    /// The name of an `end_user` tool this hook refused for lack of a verified
    /// session during the turn, if any. `Some(tool)` is the server's signal to
    /// offer OTP; `None` means nothing OTP-remediable was blocked. Cheap to poll
    /// on a clone of the hook the server retained before installing it.
    #[must_use]
    pub fn otp_refused_tool(&self) -> Option<String> {
        self.otp_refused_tool.lock().ok().and_then(|g| g.clone())
    }

    /// `true` when this hook could ever block something — i.e. some auth-supporting
    /// tool carries a non-`None` level. Lets the caller skip installing an inert
    /// hook (keeps the default tool path byte-for-byte unchanged).
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.auth_supporting_tools
            .iter()
            .any(|name| self.auth_levels.get(name).copied().unwrap_or_default() != AuthLevel::None)
    }
}

#[async_trait]
impl ToolHook for AuthGateHook {
    async fn pre_call(&self, call: &ToolCall) -> anyhow::Result<()> {
        if !self.auth_supporting_tools.contains(&call.name) {
            return Ok(());
        }
        let level = self
            .auth_levels
            .get(&call.name)
            .copied()
            .unwrap_or_default();
        match tool_auth_refusal(
            &call.name,
            level,
            self.visibility,
            self.session_authenticated,
        ) {
            Some(message) => {
                // Record the OTP-remediable refusal (public agent, `end_user`
                // tool, session not yet verified) so the server can offer a
                // verification flow after the turn. An `admin` refusal is not
                // recorded — no OTP can satisfy it.
                if level == AuthLevel::EndUser
                    && self.visibility == Visibility::Public
                    && !self.session_authenticated
                {
                    if let Ok(mut slot) = self.otp_refused_tool.lock() {
                        *slot = Some(call.name.clone());
                    }
                }
                Err(anyhow::anyhow!(message))
            }
            None => Ok(()),
        }
    }
}

/// The resolved per-agent behavior knobs. Every field is optional so a partial
/// or malformed `agents` row degrades cleanly to the org default.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentBehaviorConfig {
    /// Where the agent is reachable — gates tool auth. Defaults to `Public`.
    #[serde(default)]
    pub visibility: Visibility,
    /// `instructions.prompt` — the agent's system prompt / persona. When present
    /// it overrides the org default persona for this agent's conversations.
    pub instructions: Option<String>,
    /// `personality.persona` — an optional custom-persona addendum appended to
    /// the system prompt.
    pub persona: Option<String>,
    /// `greeting` — an optional opening line, injected into the prompt only on
    /// the first turn of a conversation (see [`greeting_section`]).
    pub greeting: Option<String>,
    /// `conversation_workflow` — the optional stepped guided flow. `None` (or a
    /// malformed / empty-steps value) means the agent runs freeform.
    pub conversation_workflow: Option<ConversationWorkflow>,
    /// `tool_config.enabledTools` — a tool allow-list. When non-empty, this
    /// agent's turns are restricted to the `enabled == true` entries' `tool_id`
    /// (empty ⇒ the full server tool set). Unknown tool ids are ignored.
    #[serde(default)]
    pub enabled_tools: Vec<EnabledTool>,
    /// `extension_config.enabledExtensions` — the per-agent SEP extension
    /// allow-list. Empty means "activates NO extension" (fail-closed): a resolved
    /// agent that lists no extensions must load zero, even when the server
    /// allowlist is non-empty. See [`enabled_extension_ids`](Self::enabled_extension_ids).
    #[serde(default)]
    pub enabled_extensions: Vec<EnabledExtension>,
    /// `model` — per-agent gateway model id override. `Some(id)` runs this agent's
    /// turns on `id` instead of the server default (`SMOOTH_AGENT_MODEL`); `None`
    /// (or an empty/whitespace column) falls back to that global default. An
    /// explicit per-turn `send_message.model` (Smooth Modes) still wins over this.
    #[serde(default)]
    pub model: Option<String>,
    /// `max_iterations` — per-agent agent-loop iteration cap. `Some(n)` overrides
    /// the server default (`SMOOTH_AGENT_MAX_ITERATIONS`) for this agent's turns;
    /// `None` falls back to that global default. Parsed values are clamped to
    /// `1..=64` (see [`from_row_values`](Self::from_row_values)).
    #[serde(default)]
    pub max_iterations: Option<u32>,
}

impl AgentBehaviorConfig {
    /// `true` when the row carried nothing usable — the runner should stay on the
    /// org default persona and take no workflow path.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.instructions.is_none()
            && self.persona.is_none()
            && self.greeting.is_none()
            && self.conversation_workflow.is_none()
            && self.enabled_tools.is_empty()
            && self.enabled_extensions.is_empty()
            && self.model.is_none()
            && self.max_iterations.is_none()
    }

    /// Build the per-agent system prompt from `instructions` (+ optional persona),
    /// or `None` when there are no `instructions` to anchor it.
    ///
    /// `None` is the signal to fall back to the org default persona — a bare
    /// persona with no instructions is not enough to define an agent. The greeting
    /// is handled separately ([`greeting_section`](Self::greeting_section)) because
    /// it is injected first-turn-only.
    #[must_use]
    pub fn system_prompt(&self) -> Option<String> {
        let instructions = self.instructions.as_deref()?.trim();
        if instructions.is_empty() {
            return None;
        }
        let mut prompt = instructions.to_string();
        if let Some(persona) = self
            .persona
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            prompt.push_str("\n\n<Personality>\n");
            prompt.push_str(persona);
            prompt.push_str("\n</Personality>");
        }
        Some(prompt)
    }

    /// The `<GreetingAwareness>` prompt section, or `None` when no greeting is set.
    /// The caller injects it only on the FIRST turn (empty prior history), so the
    /// agent opens with it once. Mirrors the sibling lanes' first-turn greeting.
    #[must_use]
    pub fn greeting_section(&self) -> Option<String> {
        let greeting = self
            .greeting
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())?;
        Some(format!(
            "<GreetingAwareness>\nThis is your first reply in this conversation. Open with a natural, brief variant of: \"{greeting}\" — then address the user's message in the same reply. Do NOT repeat the greeting verbatim, and do not reintroduce yourself later.\n</GreetingAwareness>"
        ))
    }

    /// The enabled tool-id allow-list, or `None` when unrestricted (no
    /// `tool_config` / empty `enabledTools` ⇒ the full server tool set).
    /// `Some(ids)` restricts the turn to those snake_case ids (`enabled == true`
    /// entries only); unknown ids simply match nothing.
    #[must_use]
    pub fn enabled_tool_ids(&self) -> Option<Vec<String>> {
        if self.enabled_tools.is_empty() {
            return None;
        }
        Some(
            self.enabled_tools
                .iter()
                .filter(|t| t.enabled)
                .map(|t| t.tool_id.clone())
                .collect(),
        )
    }

    /// The enabled SEP extension-id allow-list (kebab-case ids of the
    /// `enabled == true` entries). Returns a plain `Vec` — **not** an `Option` —
    /// because an empty result is meaningful: it is the fail-closed signal that a
    /// resolved agent activates NO extensions (the extension host intersects this
    /// with the server allowlist, so empty ⇒ zero extensions load). Compare with
    /// [`enabled_tool_ids`](Self::enabled_tool_ids), whose `None` means
    /// "unrestricted"; extensions default-deny per agent instead.
    #[must_use]
    pub fn enabled_extension_ids(&self) -> Vec<String> {
        self.enabled_extensions
            .iter()
            .filter(|e| e.enabled)
            .map(|e| e.extension_id.clone())
            .collect()
    }

    /// The configured [`AuthLevel`] for a tool id (from its `enabledTools`
    /// entry), or [`AuthLevel::None`] when unconfigured.
    #[must_use]
    pub fn auth_level_for(&self, tool_id: &str) -> AuthLevel {
        self.enabled_tools
            .iter()
            .find(|t| t.tool_id == tool_id)
            .map_or(AuthLevel::None, |t| AuthLevel::parse(&t.auth_level))
    }

    /// The per-tool `config` object delivered to a tool at execution (the
    /// `enabledTools` entry's `config`), for every entry that carries one. Empty
    /// when no tool has config. Mirrors `registry.ts`'s `toolSpecificConfig`.
    #[must_use]
    pub fn tool_configs(&self) -> std::collections::HashMap<String, serde_json::Value> {
        self.enabled_tools
            .iter()
            .filter(|t| !t.config.is_null())
            .map(|t| (t.tool_id.clone(), t.config.clone()))
            .collect()
    }

    /// Parse from the raw `agents`-row jsonb / text columns, tolerating any
    /// malformed shape (a bad value drops just that field — never an error).
    ///
    /// - `instructions` — jsonb `{ "prompt": string }`,
    /// - `personality` — jsonb `{ "persona"?: string, ... }`,
    /// - `greeting` — text,
    /// - `conversation_workflow` — jsonb `{ goal, steps: [...] }`,
    /// - `tool_config` — jsonb `{ enabledTools: [{ toolId, enabled, authLevel, config }] }`,
    /// - `extension_config` — jsonb `{ enabledExtensions: [{ extensionId, enabled, config }] }`,
    /// - `visibility` — text `public` | `internal` (defaults `public`),
    /// - `model` — text gateway model id (blank / whitespace ⇒ `None`, ignored),
    /// - `max_iterations` — integer loop cap, clamped to `1..=64` (out-of-range
    ///   values are clamped with a `tracing::warn`, never dropped).
    // Positional row-mapping constructor: one arg per `agents` column, mirroring
    // the sibling lanes' resolver contract. A struct-of-args would break every
    // host caller for no real gain. ponytail: flat arg list, matches the columns.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn from_row_values(
        instructions: Option<serde_json::Value>,
        personality: Option<serde_json::Value>,
        greeting: Option<String>,
        conversation_workflow: Option<serde_json::Value>,
        tool_config: Option<serde_json::Value>,
        extension_config: Option<serde_json::Value>,
        visibility: Option<String>,
        model: Option<String>,
        max_iterations: Option<i64>,
    ) -> Self {
        let visibility = visibility
            .as_deref()
            .map_or(Visibility::Public, Visibility::parse);
        let instructions = instructions
            .as_ref()
            .and_then(|v| v.get("prompt"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .filter(|s| !s.trim().is_empty());

        let persona = personality
            .as_ref()
            .and_then(|v| v.get("persona"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .filter(|s| !s.trim().is_empty());

        let greeting = greeting.filter(|s| !s.trim().is_empty());

        // A malformed workflow (wrong shape, missing fields, empty steps) parses
        // to None so the turn simply runs freeform — never a hard error.
        let conversation_workflow = conversation_workflow
            .and_then(|v| serde_json::from_value::<ConversationWorkflow>(v).ok())
            .filter(|w| !w.steps.is_empty());

        // `tool_config.enabledTools`: parse each entry tolerantly (a bad entry is
        // dropped, not fatal). camelCase keys mirror the monorepo jsonb.
        let enabled_tools = tool_config
            .as_ref()
            .and_then(|v| v.get("enabledTools"))
            .and_then(serde_json::Value::as_array)
            .map(|arr| arr.iter().filter_map(parse_enabled_tool).collect())
            .unwrap_or_default();

        // `extension_config.enabledExtensions`: same tolerant per-entry parse.
        let enabled_extensions = extension_config
            .as_ref()
            .and_then(|v| v.get("enabledExtensions"))
            .and_then(serde_json::Value::as_array)
            .map(|arr| arr.iter().filter_map(parse_enabled_extension).collect())
            .unwrap_or_default();

        // Blank / whitespace-only model column ⇒ no override (fall back to global).
        let model = model
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        // Clamp the loop cap to a sane range rather than trusting the row blindly:
        // a 0 would stall the turn, a huge value would let it run away.
        let max_iterations = max_iterations.map(clamp_max_iterations);

        Self {
            visibility,
            instructions,
            persona,
            greeting,
            conversation_workflow,
            enabled_tools,
            enabled_extensions,
            model,
            max_iterations,
        }
    }
}

/// Minimum / maximum per-agent `max_iterations` (inclusive). A turn needs at
/// least one loop; 64 is a generous ceiling that keeps a runaway agent bounded.
const MAX_ITERATIONS_RANGE: std::ops::RangeInclusive<i64> = 1..=64;

/// Clamp a raw `max_iterations` row value into [`MAX_ITERATIONS_RANGE`],
/// `tracing::warn`-ing when the raw value was out of range (so a mis-set row is
/// visible in logs rather than silently honored or dropped).
#[must_use]
fn clamp_max_iterations(raw: i64) -> u32 {
    if !MAX_ITERATIONS_RANGE.contains(&raw) {
        tracing::warn!(
            max_iterations = raw,
            "per-agent max_iterations out of range 1..=64; clamping"
        );
    }
    // `clamp` yields a value in 1..=64, which always fits u32.
    raw.clamp(*MAX_ITERATIONS_RANGE.start(), *MAX_ITERATIONS_RANGE.end()) as u32
}

/// Parse one `enabledTools` entry, tolerating missing/typed-wrong fields:
/// `toolId` is required (else the entry is dropped); `enabled` defaults `true`,
/// `authLevel` defaults `"none"`, `config` defaults `null`.
fn parse_enabled_tool(v: &serde_json::Value) -> Option<EnabledTool> {
    let tool_id = v
        .get("toolId")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty())?;
    Some(EnabledTool {
        tool_id,
        enabled: v
            .get("enabled")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true),
        auth_level: v
            .get("authLevel")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("none")
            .to_string(),
        config: v.get("config").cloned().unwrap_or(serde_json::Value::Null),
    })
}

/// Parse one `enabledExtensions` entry, tolerating missing/typed-wrong fields:
/// `extensionId` is required (else the entry is dropped); `enabled` defaults
/// `true`, `config` defaults absent.
fn parse_enabled_extension(v: &serde_json::Value) -> Option<EnabledExtension> {
    let extension_id = v
        .get("extensionId")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty())?;
    Some(EnabledExtension {
        extension_id,
        enabled: v
            .get("enabled")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true),
        config: v.get("config").cloned(),
    })
}

// ---------------------------------------------------------------------------
// Workflow step resolution + rendering (parity with workflow.ts)
// ---------------------------------------------------------------------------

/// Resolve the current step for a `(workflow, pointer)` pair.
///
/// - Pointer matches a step id → that step.
/// - Pointer empty / unknown → the first step (fresh start).
/// - Empty workflow → `None`.
#[must_use]
pub fn resolve_current_step<'a>(
    workflow: &'a ConversationWorkflow,
    current_step_id: Option<&str>,
) -> Option<&'a ConversationWorkflowStep> {
    if workflow.steps.is_empty() {
        return None;
    }
    if let Some(id) = current_step_id {
        if let Some(found) = workflow.steps.iter().find(|s| s.id == id) {
            return Some(found);
        }
    }
    workflow.steps.first()
}

/// The step to advance to once `current` is satisfied. Preference order:
///   1. explicit `current.next` if it resolves to a known step id,
///   2. the element immediately following `current`,
///   3. `None` — workflow complete (terminal step).
#[must_use]
pub fn next_step<'a>(
    workflow: &'a ConversationWorkflow,
    current: &ConversationWorkflowStep,
) -> Option<&'a ConversationWorkflowStep> {
    if let Some(next_id) = current.next.as_deref().filter(|s| !s.is_empty()) {
        if let Some(explicit) = workflow.steps.iter().find(|s| s.id == next_id) {
            return Some(explicit);
        }
    }
    let idx = workflow.steps.iter().position(|s| s.id == current.id)?;
    workflow.steps.get(idx + 1)
}

/// Render the current step as a `<ConversationWorkflow>` block for the system
/// prompt. Empty string when there is no resolvable step, so the caller can
/// concatenate unconditionally. Mirrors `renderWorkflowPromptSection`.
#[must_use]
pub fn render_workflow_prompt_section(
    workflow: &ConversationWorkflow,
    current_step_id: Option<&str>,
) -> String {
    let Some(step) = resolve_current_step(workflow, current_step_id) else {
        return String::new();
    };
    let idx = workflow
        .steps
        .iter()
        .position(|s| s.id == step.id)
        .unwrap_or(0);
    let step_number = idx + 1;
    let total = workflow.steps.len();
    format!(
        "<ConversationWorkflow>\nGOAL: {goal}\n\nCURRENT STEP ({step_number}/{total}): {id}\nINTENT: {intent}\nCRITERIA: {criteria}\n\nFocus this turn on the CURRENT STEP. Pursue the INTENT and aim to satisfy the CRITERIA. You don't have to force the step to close if the user isn't ready — stay conversational and the workflow will advance once the criteria are clearly met.\n</ConversationWorkflow>",
        goal = workflow.goal,
        id = step.id,
        intent = step.intent,
        criteria = step.criteria,
    )
}

// ---------------------------------------------------------------------------
// Judge (parity with workflow-judge.ts)
// ---------------------------------------------------------------------------

/// The workflow judge's verdict on whether the current step's criteria were met
/// this turn. Mirrors `WorkflowJudgeVerdict`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowJudgeVerdict {
    /// Criteria clearly satisfied — advance.
    Yes,
    /// Not satisfied — stay on the current step.
    No,
    /// Partial / ambiguous — stay on the current step, try again next turn.
    Maybe,
    /// No workflow / nothing to evaluate.
    Skipped,
}

impl WorkflowJudgeVerdict {
    /// Parse a judge model's free-text reply into a verdict. Lenient: matches the
    /// first of `yes` / `no` / `maybe` found (case-insensitive, word-ish), so it
    /// survives a model that wraps the word in punctuation or a short sentence.
    /// Anything unrecognized → [`Maybe`](Self::Maybe) (stay put, don't over-advance).
    #[must_use]
    pub fn parse(reply: &str) -> Self {
        let lower = reply.trim().to_lowercase();
        // Order matters: "maybe" contains neither "yes" nor "no", but check it
        // first so a reply like "maybe not" resolves to Maybe, not No.
        if lower.contains("maybe") {
            return Self::Maybe;
        }
        if lower.contains("yes") {
            return Self::Yes;
        }
        if lower.contains("no") {
            return Self::No;
        }
        Self::Maybe
    }
}

/// The judge's system prompt. Kept as a const so tests and the runner share the
/// exact wording. Mirrors the TS judge's rubric.
pub const JUDGE_SYSTEM_PROMPT: &str = "You are a conversation-workflow judge. Given the CURRENT STEP's intent + criteria and the most recent agent reply, decide whether the step was satisfied this turn.\n\nRules:\n- \"yes\" -> the criteria are clearly satisfied on the basis of this turn.\n- \"no\" -> not satisfied, or the agent moved away from the step.\n- \"maybe\" -> partial/ambiguous progress; stay on the current step and try again next turn.\n- Only answer \"yes\" when the criteria are objectively met. It is OK to stay on a step for multiple turns.\n\nReply with EXACTLY one word: yes, no, or maybe.";

/// Build the judge's user prompt for one turn. Mirrors the TS human prompt.
#[must_use]
pub fn judge_user_prompt(
    workflow: &ConversationWorkflow,
    step: &ConversationWorkflowStep,
    user_message: &str,
    agent_reply: &str,
) -> String {
    format!(
        "GOAL: {goal}\n\nCURRENT STEP ({id}):\n  intent: {intent}\n  criteria: {criteria}\n\nLAST USER MESSAGE:\n{user}\n\nAGENT REPLY:\n{reply}\n\nReturn exactly one word: yes, no, or maybe.",
        goal = workflow.goal,
        id = step.id,
        intent = step.intent,
        criteria = step.criteria,
        user = if user_message.is_empty() { "(none)" } else { user_message },
        reply = agent_reply,
    )
}

/// Compute the tracked step id after a judge verdict. `Yes` advances (to
/// [`next_step`], or stays put on a terminal step); every other verdict stays on
/// the current step. Never freezes: an unresolvable pointer resolves to the
/// first step. Returns `None` only for an empty workflow.
#[must_use]
pub fn advance_after_verdict(
    workflow: &ConversationWorkflow,
    current_step_id: Option<&str>,
    verdict: WorkflowJudgeVerdict,
) -> Option<String> {
    let current = resolve_current_step(workflow, current_step_id)?;
    if verdict == WorkflowJudgeVerdict::Yes {
        if let Some(next) = next_step(workflow, current) {
            return Some(next.id.clone());
        }
    }
    Some(current.id.clone())
}

/// Default per-step attempt cap: after this many consecutive non-advancing turns
/// on the same step, [`apply_step_cap`] force-advances so a step whose criteria
/// the judge never accepts can't loop forever (public-agent pathological visitor;
/// th-d57a1d). The counter resets whenever the step advances.
pub const WORKFLOW_STEP_ATTEMPT_CAP: u32 = 3;

/// Enforce the per-step attempt cap on top of the judge's verdict.
///
/// `judged_next` is the step id [`advance_after_verdict`] already produced this
/// turn; `step_before` is the step the session was on at turn start; `attempts`
/// is the consecutive-non-advancing count carried from the prior turn. Returns
/// the `(step_id, attempts)` to persist:
///   - the judge advanced this turn → reset the counter to 0,
///   - otherwise increment; once it reaches `max_attempts` force-advance to the
///     next step (counter resets), unless the step is terminal (nothing to
///     advance to → stay, counter keeps climbing but is harmless).
///
/// Comparing against the *resolved* current step (not raw `step_before`) makes a
/// fresh `None` pointer count its first turn on step 1 as a real attempt, so
/// every step tolerates exactly `max_attempts` non-advancing turns.
#[must_use]
pub fn apply_step_cap(
    workflow: &ConversationWorkflow,
    step_before: Option<&str>,
    judged_next: &str,
    attempts: u32,
    max_attempts: u32,
) -> (String, u32) {
    let current = resolve_current_step(workflow, step_before).map(|s| s.id.as_str());
    if current != Some(judged_next) {
        // The judge advanced (or moved) the step this turn → reset the counter.
        return (judged_next.to_string(), 0);
    }
    let next_attempts = attempts + 1;
    if next_attempts >= max_attempts {
        if let Some(cur) = resolve_current_step(workflow, Some(judged_next)) {
            if let Some(next) = next_step(workflow, cur) {
                return (next.id.clone(), 0);
            }
        }
    }
    (judged_next.to_string(), next_attempts)
}

// ---------------------------------------------------------------------------
// Provider seam
// ---------------------------------------------------------------------------

/// Seam for resolving an agent's [`AgentBehaviorConfig`] by `agent_id`.
///
/// The ws protocol's `create_conversation_session` carries only an agent UUID, so
/// per-agent config is looked up **server-side by id**. Implemented by the host
/// (backed by the monorepo `agents` table). Returning `None` means "no per-agent
/// config" — the runner falls back to the org default persona, exactly as before
/// this seam existed. Matches the sibling lanes' `AgentConfigResolver.resolve`.
#[async_trait]
pub trait AgentConfigResolver: Send + Sync {
    /// The per-agent behavior config for `agent_id`, or `None` when the agent is
    /// unknown / has no usable config.
    async fn resolve(&self, agent_id: &str) -> Option<AgentBehaviorConfig>;
}

/// Static map resolver (`agentId` → config), for tests and DB-free hosts. The
/// empty default is the server's no-op resolver (every agent → `None`), so the
/// reference/OSS server stays on its org-default behavior.
#[derive(Debug, Default)]
pub struct StaticAgentConfigResolver {
    rows: std::collections::HashMap<String, AgentBehaviorConfig>,
}

impl StaticAgentConfigResolver {
    /// Build from an in-memory map.
    #[must_use]
    pub fn new(rows: std::collections::HashMap<String, AgentBehaviorConfig>) -> Self {
        Self { rows }
    }

    /// Insert / replace one agent's config (builder style).
    #[must_use]
    pub fn with(mut self, agent_id: impl Into<String>, config: AgentBehaviorConfig) -> Self {
        self.rows.insert(agent_id.into(), config);
        self
    }
}

#[async_trait]
impl AgentConfigResolver for StaticAgentConfigResolver {
    async fn resolve(&self, agent_id: &str) -> Option<AgentBehaviorConfig> {
        self.rows.get(agent_id).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn wf() -> ConversationWorkflow {
        ConversationWorkflow {
            goal: "Assess posture".into(),
            steps: vec![
                ConversationWorkflowStep {
                    id: "greet".into(),
                    intent: "Greet and confirm name".into(),
                    criteria: "User's name captured".into(),
                    next: None,
                },
                ConversationWorkflowStep {
                    id: "collect".into(),
                    intent: "Collect current tooling".into(),
                    criteria: "At least one tool named".into(),
                    next: Some("summary".into()),
                },
                ConversationWorkflowStep {
                    id: "summary".into(),
                    intent: "Summarize".into(),
                    criteria: "Summary delivered".into(),
                    next: None,
                },
            ],
        }
    }

    #[test]
    fn resolve_current_step_defaults_to_first() {
        let w = wf();
        assert_eq!(resolve_current_step(&w, None).unwrap().id, "greet");
        assert_eq!(
            resolve_current_step(&w, Some("unknown")).unwrap().id,
            "greet"
        );
        assert_eq!(
            resolve_current_step(&w, Some("collect")).unwrap().id,
            "collect"
        );
    }

    #[test]
    fn resolve_current_step_empty_workflow_is_none() {
        let empty = ConversationWorkflow {
            goal: "g".into(),
            steps: vec![],
        };
        assert!(resolve_current_step(&empty, None).is_none());
    }

    #[test]
    fn next_step_prefers_explicit_then_sequential_then_terminal() {
        let w = wf();
        // greet has no `next` → sequential → collect
        let greet = &w.steps[0];
        assert_eq!(next_step(&w, greet).unwrap().id, "collect");
        // collect.next = summary (explicit, also happens to be sequential here)
        let collect = &w.steps[1];
        assert_eq!(next_step(&w, collect).unwrap().id, "summary");
        // summary is terminal
        let summary = &w.steps[2];
        assert!(next_step(&w, summary).is_none());
    }

    #[test]
    fn next_step_explicit_jump_overrides_order() {
        let w = ConversationWorkflow {
            goal: "g".into(),
            steps: vec![
                ConversationWorkflowStep {
                    id: "a".into(),
                    intent: "i".into(),
                    criteria: "c".into(),
                    next: Some("c".into()), // skip b
                },
                ConversationWorkflowStep {
                    id: "b".into(),
                    intent: "i".into(),
                    criteria: "c".into(),
                    next: None,
                },
                ConversationWorkflowStep {
                    id: "c".into(),
                    intent: "i".into(),
                    criteria: "c".into(),
                    next: None,
                },
            ],
        };
        assert_eq!(next_step(&w, &w.steps[0]).unwrap().id, "c");
    }

    #[test]
    fn next_step_unknown_explicit_next_falls_through_to_sequential() {
        let w = ConversationWorkflow {
            goal: "g".into(),
            steps: vec![
                ConversationWorkflowStep {
                    id: "a".into(),
                    intent: "i".into(),
                    criteria: "c".into(),
                    next: Some("nonexistent".into()),
                },
                ConversationWorkflowStep {
                    id: "b".into(),
                    intent: "i".into(),
                    criteria: "c".into(),
                    next: None,
                },
            ],
        };
        assert_eq!(next_step(&w, &w.steps[0]).unwrap().id, "b");
    }

    #[test]
    fn render_section_includes_goal_intent_criteria_and_position() {
        let w = wf();
        let section = render_workflow_prompt_section(&w, Some("collect"));
        assert!(section.contains("GOAL: Assess posture"));
        assert!(section.contains("CURRENT STEP (2/3): collect"));
        assert!(section.contains("INTENT: Collect current tooling"));
        assert!(section.contains("CRITERIA: At least one tool named"));
    }

    #[test]
    fn render_section_empty_workflow_is_empty_string() {
        let empty = ConversationWorkflow {
            goal: "g".into(),
            steps: vec![],
        };
        assert_eq!(render_workflow_prompt_section(&empty, None), "");
    }

    #[test]
    fn verdict_parse_is_lenient() {
        assert_eq!(
            WorkflowJudgeVerdict::parse("yes"),
            WorkflowJudgeVerdict::Yes
        );
        assert_eq!(
            WorkflowJudgeVerdict::parse("YES."),
            WorkflowJudgeVerdict::Yes
        );
        assert_eq!(
            WorkflowJudgeVerdict::parse("Yes, criteria met"),
            WorkflowJudgeVerdict::Yes
        );
        assert_eq!(WorkflowJudgeVerdict::parse("no"), WorkflowJudgeVerdict::No);
        assert_eq!(
            WorkflowJudgeVerdict::parse("maybe"),
            WorkflowJudgeVerdict::Maybe
        );
        // "maybe not" must resolve to Maybe (not No) — maybe is checked first.
        assert_eq!(
            WorkflowJudgeVerdict::parse("maybe not"),
            WorkflowJudgeVerdict::Maybe
        );
        // Unrecognized → Maybe (conservative: don't advance).
        assert_eq!(
            WorkflowJudgeVerdict::parse("???"),
            WorkflowJudgeVerdict::Maybe
        );
    }

    #[test]
    fn advance_only_on_yes() {
        let w = wf();
        assert_eq!(
            advance_after_verdict(&w, Some("greet"), WorkflowJudgeVerdict::Yes).as_deref(),
            Some("collect")
        );
        assert_eq!(
            advance_after_verdict(&w, Some("greet"), WorkflowJudgeVerdict::No).as_deref(),
            Some("greet")
        );
        assert_eq!(
            advance_after_verdict(&w, Some("greet"), WorkflowJudgeVerdict::Maybe).as_deref(),
            Some("greet")
        );
    }

    #[test]
    fn advance_on_terminal_step_stays_put() {
        let w = wf();
        assert_eq!(
            advance_after_verdict(&w, Some("summary"), WorkflowJudgeVerdict::Yes).as_deref(),
            Some("summary")
        );
    }

    #[test]
    fn advance_from_fresh_pointer_starts_at_first() {
        let w = wf();
        // None pointer resolves to first step "greet"; yes advances to "collect".
        assert_eq!(
            advance_after_verdict(&w, None, WorkflowJudgeVerdict::Yes).as_deref(),
            Some("collect")
        );
    }

    #[test]
    fn step_cap_yes_advances_immediately_and_resets() {
        let w = wf();
        // Judge advanced greet→collect this turn: reset the counter regardless of
        // the carried attempts.
        assert_eq!(
            apply_step_cap(&w, Some("greet"), "collect", 2, WORKFLOW_STEP_ATTEMPT_CAP),
            ("collect".to_string(), 0)
        );
    }

    #[test]
    fn step_cap_single_hold_stays_and_counts() {
        let w = wf();
        // Judge held on greet (judged_next == current): one non-advancing turn.
        assert_eq!(
            apply_step_cap(&w, Some("greet"), "greet", 0, WORKFLOW_STEP_ATTEMPT_CAP),
            ("greet".to_string(), 1)
        );
    }

    #[test]
    fn step_cap_force_advances_on_third_consecutive_hold() {
        let w = wf();
        // Two holds already counted; the third reaches the cap → force-advance.
        assert_eq!(
            apply_step_cap(&w, Some("greet"), "greet", 2, WORKFLOW_STEP_ATTEMPT_CAP),
            ("collect".to_string(), 0)
        );
    }

    #[test]
    fn step_cap_fresh_pointer_counts_first_turn_on_step_one() {
        let w = wf();
        // None resolves to "greet"; a hold on the very first turn counts as attempt 1.
        assert_eq!(
            apply_step_cap(&w, None, "greet", 0, WORKFLOW_STEP_ATTEMPT_CAP),
            ("greet".to_string(), 1)
        );
    }

    #[test]
    fn step_cap_terminal_step_cannot_advance() {
        let w = wf();
        // Terminal step at the cap has nowhere to go → stay, keep counting.
        assert_eq!(
            apply_step_cap(&w, Some("summary"), "summary", 2, WORKFLOW_STEP_ATTEMPT_CAP),
            ("summary".to_string(), 3)
        );
    }

    #[test]
    fn system_prompt_requires_instructions() {
        // Persona / greeting alone do NOT override the org default.
        let cfg = AgentBehaviorConfig {
            instructions: None,
            persona: Some("snarky".into()),
            greeting: Some("hi".into()),
            ..Default::default()
        };
        assert!(cfg.system_prompt().is_none());
    }

    #[test]
    fn system_prompt_composes_instructions_and_personality() {
        let cfg = AgentBehaviorConfig {
            instructions: Some("You are the Posture assistant.".into()),
            persona: Some("Warm and direct.".into()),
            greeting: Some("Welcome!".into()),
            ..Default::default()
        };
        let p = cfg.system_prompt().unwrap();
        assert!(p.starts_with("You are the Posture assistant."));
        assert!(p.contains("<Personality>"));
        assert!(p.contains("Warm and direct."));
        // Greeting is NOT in the system prompt — it is first-turn-only.
        assert!(!p.contains("Welcome!"));
        // ...and is available separately for the runner to inject on turn 1.
        assert!(cfg.greeting_section().unwrap().contains("Welcome!"));
    }

    #[test]
    fn from_row_values_parses_well_formed_row() {
        let cfg = AgentBehaviorConfig::from_row_values(
            Some(
                json!({ "prompt": "You are the Posture assistant. NOT a generic support agent." }),
            ),
            Some(json!({ "preset": "professional", "creativity": 0.5, "persona": "Warm." })),
            Some("Hey there".into()),
            Some(json!({
                "goal": "Assess",
                "steps": [
                    { "id": "greet", "intent": "greet", "criteria": "name captured" }
                ]
            })),
            Some(json!({
                "enabledTools": [
                    { "toolId": "knowledge_search", "enabled": true, "authLevel": "none" },
                    { "toolId": "admin_tool", "enabled": true, "authLevel": "admin", "config": { "k": 1 } },
                    { "toolId": "notify_humans", "enabled": false }
                ]
            })),
            Some(json!({
                "enabledExtensions": [
                    { "extensionId": "plan-mode", "enabled": true, "config": { "x": 1 } },
                    { "extensionId": "gate", "enabled": true },
                    { "extensionId": "disabled-ext", "enabled": false }
                ]
            })),
            Some("internal".into()),
            Some("claude-opus-4-8".into()),
            Some(12),
        );
        assert_eq!(
            cfg.instructions.as_deref(),
            Some("You are the Posture assistant. NOT a generic support agent.")
        );
        assert_eq!(cfg.model.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(cfg.max_iterations, Some(12));
        assert_eq!(cfg.persona.as_deref(), Some("Warm."));
        assert_eq!(cfg.greeting.as_deref(), Some("Hey there"));
        assert_eq!(cfg.visibility, Visibility::Internal);
        let wf = cfg.conversation_workflow.clone().unwrap();
        assert_eq!(wf.goal, "Assess");
        assert_eq!(wf.steps.len(), 1);
        assert_eq!(wf.steps[0].id, "greet");
        // enabledTools parsed; only enabled=true entries are in the allow-list.
        assert_eq!(cfg.enabled_tools.len(), 3);
        assert_eq!(
            cfg.enabled_tool_ids(),
            Some(vec![
                "knowledge_search".to_string(),
                "admin_tool".to_string()
            ])
        );
        // Per-tool authLevel + config are parsed.
        assert_eq!(cfg.auth_level_for("admin_tool"), AuthLevel::Admin);
        assert_eq!(cfg.auth_level_for("knowledge_search"), AuthLevel::None);
        assert_eq!(
            cfg.tool_configs().get("admin_tool"),
            Some(&json!({ "k": 1 }))
        );
        // enabledExtensions parsed; only enabled=true entries in the id list.
        assert_eq!(cfg.enabled_extensions.len(), 3);
        assert_eq!(
            cfg.enabled_extension_ids(),
            vec!["plan-mode".to_string(), "gate".to_string()]
        );
    }

    #[test]
    fn enabled_tool_ids_none_when_no_tool_config() {
        let cfg = AgentBehaviorConfig::from_row_values(
            Some(json!({ "prompt": "hi" })),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        );
        // No tool_config → unrestricted (full server tool set).
        assert!(cfg.enabled_tool_ids().is_none());
    }

    #[test]
    fn enabled_extension_ids_empty_when_no_extension_config() {
        let cfg = AgentBehaviorConfig::from_row_values(
            Some(json!({ "prompt": "hi" })),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        );
        // No extension_config → empty Vec (NOT None): a resolved agent that lists
        // no extensions activates ZERO — the fail-closed signal, distinct from
        // `enabled_tool_ids`'s "None ⇒ unrestricted".
        assert!(cfg.enabled_extension_ids().is_empty());
    }

    #[test]
    fn from_row_values_tolerates_malformed_jsonb() {
        // instructions not an object, personality a string, workflow missing
        // `steps`, greeting blank → every field degrades to None, no panic.
        let cfg = AgentBehaviorConfig::from_row_values(
            Some(json!("just a string")),
            Some(json!("not an object")),
            Some("   ".into()),
            Some(json!({ "goal": "no steps here" })),
            Some(json!("tool_config not an object")),
            Some(json!("extension_config not an object")),
            Some("garbage-visibility".into()),
            Some("   ".into()),
            None,
        );
        assert!(
            cfg.is_empty(),
            "malformed row must degrade to empty config: {cfg:?}"
        );
        // Blank model column ⇒ no override (contributes to is_empty()).
        assert!(cfg.model.is_none());
        // Unknown visibility string → default public (never an error).
        assert_eq!(cfg.visibility, Visibility::Public);
    }

    #[test]
    fn from_row_values_drops_empty_steps_workflow() {
        let cfg = AgentBehaviorConfig::from_row_values(
            Some(json!({ "prompt": "hi" })),
            None,
            None,
            Some(json!({ "goal": "g", "steps": [] })),
            None,
            None,
            None,
            None,
            None,
        );
        assert!(cfg.conversation_workflow.is_none());
        assert_eq!(cfg.instructions.as_deref(), Some("hi"));
    }

    #[test]
    fn from_row_values_reads_model_and_clamps_iterations() {
        // In range: kept verbatim.
        let ok = AgentBehaviorConfig::from_row_values(
            Some(json!({ "prompt": "hi" })),
            None,
            None,
            None,
            None,
            None,
            None,
            Some("groq/qwen".into()),
            Some(8),
        );
        assert_eq!(ok.model.as_deref(), Some("groq/qwen"));
        assert_eq!(ok.max_iterations, Some(8));

        // Zero / negative clamps up to 1; huge clamps down to 64.
        let low = AgentBehaviorConfig::from_row_values(
            Some(json!({ "prompt": "hi" })),
            None,
            None,
            None,
            None,
            None,
            None,
            Some("  ".into()), // blank model ⇒ None
            Some(0),
        );
        assert_eq!(low.model, None);
        assert_eq!(low.max_iterations, Some(1));

        let high = AgentBehaviorConfig::from_row_values(
            Some(json!({ "prompt": "hi" })),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(10_000),
        );
        assert_eq!(high.max_iterations, Some(64));
    }

    #[test]
    fn auth_refusal_mirrors_reference_branches() {
        // internal agent → any level satisfied.
        assert!(tool_auth_refusal("t", AuthLevel::Admin, Visibility::Internal, false).is_none());
        assert!(tool_auth_refusal("t", AuthLevel::EndUser, Visibility::Internal, false).is_none());
        // public + none → allowed.
        assert!(tool_auth_refusal("t", AuthLevel::None, Visibility::Public, false).is_none());
        // public + admin → refuse.
        let admin =
            tool_auth_refusal("admin_tool", AuthLevel::Admin, Visibility::Public, false).unwrap();
        assert!(admin.contains("requires admin authentication"));
        // public + end_user, unauthenticated → refuse asking for verification.
        let eu = tool_auth_refusal("pay", AuthLevel::EndUser, Visibility::Public, false).unwrap();
        assert!(eu.contains("verify your identity"));
        // public + end_user, authenticated → allowed.
        assert!(tool_auth_refusal("pay", AuthLevel::EndUser, Visibility::Public, true).is_none());
    }

    #[tokio::test]
    async fn auth_gate_hook_only_gates_supporting_tools() {
        let levels: HashMap<String, AuthLevel> = [("pay".to_string(), AuthLevel::Admin)]
            .into_iter()
            .collect();
        let supporting: HashSet<String> = ["pay".to_string()].into_iter().collect();
        let hook = AuthGateHook::new(levels, Visibility::Public, false, supporting);
        assert!(hook.is_active());

        // The gated admin tool on a public agent is blocked.
        let pay = ToolCall {
            id: "1".into(),
            name: "pay".into(),
            arguments: serde_json::json!({}),
        };
        assert!(hook.pre_call(&pay).await.is_err());

        // A tool NOT in the supporting set is never gated, even with a level.
        let ks = ToolCall {
            id: "2".into(),
            name: "knowledge_search".into(),
            arguments: serde_json::json!({}),
        };
        assert!(hook.pre_call(&ks).await.is_ok());
    }

    #[tokio::test]
    async fn auth_gate_records_end_user_refusal_for_otp() {
        // A public agent, an `end_user` tool, an unverified session → the refusal
        // is OTP-remediable, so the hook records the tool name.
        let levels: HashMap<String, AuthLevel> = [("pay".to_string(), AuthLevel::EndUser)]
            .into_iter()
            .collect();
        let supporting: HashSet<String> = ["pay".to_string()].into_iter().collect();
        let hook = AuthGateHook::new(levels, Visibility::Public, false, supporting);

        assert_eq!(hook.otp_refused_tool(), None, "nothing refused yet");
        let pay = ToolCall {
            id: "1".into(),
            name: "pay".into(),
            arguments: serde_json::json!({}),
        };
        assert!(hook.pre_call(&pay).await.is_err());
        assert_eq!(hook.otp_refused_tool(), Some("pay".to_string()));
    }

    #[tokio::test]
    async fn auth_gate_does_not_record_admin_refusal_for_otp() {
        // An `admin` refusal on a public agent is not OTP-remediable, so it is
        // NOT recorded — the server must not offer OTP for it.
        let levels: HashMap<String, AuthLevel> = [("admin_tool".to_string(), AuthLevel::Admin)]
            .into_iter()
            .collect();
        let supporting: HashSet<String> = ["admin_tool".to_string()].into_iter().collect();
        let hook = AuthGateHook::new(levels, Visibility::Public, false, supporting);

        let call = ToolCall {
            id: "1".into(),
            name: "admin_tool".into(),
            arguments: serde_json::json!({}),
        };
        assert!(hook.pre_call(&call).await.is_err());
        assert_eq!(hook.otp_refused_tool(), None);
    }

    #[tokio::test]
    async fn auth_gate_records_nothing_when_session_verified() {
        // A verified session passes the `end_user` gate → no refusal recorded.
        let levels: HashMap<String, AuthLevel> = [("pay".to_string(), AuthLevel::EndUser)]
            .into_iter()
            .collect();
        let supporting: HashSet<String> = ["pay".to_string()].into_iter().collect();
        let hook = AuthGateHook::new(levels, Visibility::Public, true, supporting);

        let pay = ToolCall {
            id: "1".into(),
            name: "pay".into(),
            arguments: serde_json::json!({}),
        };
        assert!(hook.pre_call(&pay).await.is_ok());
        assert_eq!(hook.otp_refused_tool(), None);
    }

    #[test]
    fn auth_gate_inactive_when_no_supporting_tool_has_a_level() {
        // A supporting tool with authLevel none, and a leveled tool that isn't
        // supporting → nothing to gate.
        let levels: HashMap<String, AuthLevel> = [("admin_tool".to_string(), AuthLevel::Admin)]
            .into_iter()
            .collect();
        let supporting: HashSet<String> = ["knowledge_search".to_string()].into_iter().collect();
        let hook = AuthGateHook::new(levels, Visibility::Public, false, supporting);
        assert!(!hook.is_active());
    }

    #[tokio::test]
    async fn empty_resolver_returns_none() {
        assert!(StaticAgentConfigResolver::default()
            .resolve("anything")
            .await
            .is_none());
    }

    #[tokio::test]
    async fn static_provider_is_per_agent_isolated() {
        let provider = StaticAgentConfigResolver::default()
            .with(
                "agent-a",
                AgentBehaviorConfig {
                    instructions: Some("A persona".into()),
                    ..Default::default()
                },
            )
            .with(
                "agent-b",
                AgentBehaviorConfig {
                    instructions: Some("B persona".into()),
                    ..Default::default()
                },
            );
        assert_eq!(
            provider
                .resolve("agent-a")
                .await
                .unwrap()
                .instructions
                .as_deref(),
            Some("A persona")
        );
        assert_eq!(
            provider
                .resolve("agent-b")
                .await
                .unwrap()
                .instructions
                .as_deref(),
            Some("B persona")
        );
        assert!(provider.resolve("agent-c").await.is_none());
    }
}
