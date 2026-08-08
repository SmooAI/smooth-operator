//! MCP client — Model Context Protocol servers as engine tools.
//!
//! A host (Big Smooth's daemon, a CLI, a service flavor) hands this module a
//! list of [`McpServerConfig`]s; [`McpToolProvider`] connects to each server,
//! lists its tools, and returns them as engine [`Tool`]s namespaced
//! `mcp__<server>__<tool>`. Because it goes through the normal
//! [`ToolProvider`] seam, MCP tools land in the SAME per-turn `ToolRegistry` as
//! the built-ins — so an embedding host's `ToolHook`s (permission gate, Narc)
//! apply to them with no extra wiring.
//!
//! ## Config
//!
//! The TOML shape is the one Smooth already ships in `~/.smooth/mcp.toml` and
//! `<repo>/.smooth/mcp.toml` — a `[[servers]]` array with `name` / `command` /
//! `args` / `env` / `disabled`, extended here with `url` + `bearer_token` for
//! the streamable-HTTP transport. Project entries shadow global ones by name
//! ([`McpConfig::merge`]).
//!
//! Secrets are never literals: `env`, `args`, `url` and `bearer_token` all go
//! through [`expand_env`], so `bearer_token = "${env:SMOO_TOKEN}"` resolves
//! from the host process environment at connect time.
//!
//! ## Failure model
//!
//! A server that will not connect (or will not list its tools) is **skipped
//! with a warning** — its tools are simply absent from the turn. An MCP server
//! is a third-party process; it must never be able to take the engine down.
//!
//! ponytail: v1 is tools only — no resources, prompts, sampling, or
//! `notifications/tools/list_changed`. The tool list is fetched once per
//! provider and cached for the process lifetime. Upgrade path: keep the
//! `RunningService` handles (already held by each [`McpTool`]) and add a
//! `resources()` accessor + a list-changed subscription that invalidates
//! `McpToolProvider::tools`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use rmcp::model::{CallToolRequestParams, CallToolResult, RawContent};
use rmcp::service::RunningService;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use smooth_operator_core::{Tool, ToolSchema};
use tokio::sync::OnceCell;

use crate::tool_provider::{ToolProvider, ToolProviderContext};

/// How long a single `tools/call` may take before it is abandoned. Overridable
/// per server via [`McpServerConfig::timeout_secs`].
pub const DEFAULT_CALL_TIMEOUT_SECS: u64 = 60;

/// The prefix every MCP-sourced tool name carries.
pub const TOOL_NAME_PREFIX: &str = "mcp__";

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// The `mcp.toml` document: a list of MCP servers.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct McpConfig {
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
}

/// One MCP server. Either a spawned stdio process (`command` + `args`) or a
/// streamable-HTTP endpoint (`url`); `url` wins if both are set.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct McpServerConfig {
    /// Namespace for this server's tools (`mcp__<name>__<tool>`).
    pub name: String,
    /// stdio transport: the binary to spawn. Ignored when `url` is set.
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment for the spawned process. Values pass through
    /// [`expand_env`].
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Streamable-HTTP transport endpoint (e.g. `https://mcp.smoo.ai/mcp`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Bearer token for `url`, sent as `Authorization: Bearer …`. Write it as
    /// `${env:VAR}` — a literal here is a secret in a config file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer_token: Option<String>,
    /// Registered but not started.
    #[serde(default)]
    pub disabled: bool,
    /// Per-call timeout override; [`DEFAULT_CALL_TIMEOUT_SECS`] when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

impl McpConfig {
    /// Parse a `mcp.toml`. A missing file is an empty config — an
    /// unconfigured host is not an error.
    ///
    /// # Errors
    /// Returns an error if the file exists but cannot be read or parsed.
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw =
            std::fs::read_to_string(path).map_err(|e| anyhow!("read {}: {e}", path.display()))?;
        toml::from_str(&raw).map_err(|e| anyhow!("parse {}: {e}", path.display()))
    }

    /// Merge a global config with a project one. A project server **shadows**
    /// a global server of the same name (the editorconfig/direnv rule Smooth
    /// documents for `.smooth/mcp.toml`).
    #[must_use]
    pub fn merge(global: Self, project: Self) -> Vec<McpServerConfig> {
        let mut out = global.servers;
        for p in project.servers {
            match out.iter_mut().find(|g| g.name == p.name) {
                Some(slot) => *slot = p,
                None => out.push(p),
            }
        }
        out
    }

    /// [`load`](Self::load) both scopes and [`merge`](Self::merge) them.
    ///
    /// # Errors
    /// Returns an error if either file exists but cannot be read or parsed.
    pub fn load_merged(global: &Path, project: &Path) -> Result<Vec<McpServerConfig>> {
        Ok(Self::merge(Self::load(global)?, Self::load(project)?))
    }
}

/// Expand `${env:VAR}` references from the process environment. Unset
/// variables expand to empty; an unterminated reference passes through
/// verbatim. Same convention (and behavior) as Smooth's `th mcp add -e`.
#[must_use]
pub fn expand_env(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(idx) = rest.find("${env:") {
        out.push_str(&rest[..idx]);
        let after = &rest[idx + "${env:".len()..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[idx..]);
            return out;
        };
        out.push_str(&std::env::var(&after[..end]).unwrap_or_default());
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// The engine-facing name for `tool` on `server`: `mcp__<server>__<tool>`,
/// with anything outside `[A-Za-z0-9_-]` folded to `_` so the name survives
/// provider-side tool-name validation.
#[must_use]
pub fn tool_name(server: &str, tool: &str) -> String {
    format!("{TOOL_NAME_PREFIX}{}__{}", sanitize(server), sanitize(tool))
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Connection + tool
// ---------------------------------------------------------------------------

type McpClient = RunningService<RoleClient, ()>;

/// Connect to one server and return the live client handle.
async fn connect(cfg: &McpServerConfig) -> Result<McpClient> {
    if let Some(url) = &cfg.url {
        let mut transport_cfg = StreamableHttpClientTransportConfig::with_uri(expand_env(url));
        if let Some(token) = &cfg.bearer_token {
            let token = expand_env(token);
            if !token.is_empty() {
                transport_cfg = transport_cfg.auth_header(token);
            }
        }
        // `from_config` is the reqwest-specialized constructor — it owns the
        // HTTP client, so rmcp's reqwest never has to match ours.
        Ok(
            ().serve(StreamableHttpClientTransport::from_config(transport_cfg))
                .await?,
        )
    } else {
        if cfg.command.is_empty() {
            return Err(anyhow!(
                "server `{}` has neither `command` nor `url`",
                cfg.name
            ));
        }
        let mut cmd = tokio::process::Command::new(expand_env(&cfg.command));
        for arg in &cfg.args {
            cmd.arg(expand_env(arg));
        }
        for (k, v) in &cfg.env {
            cmd.env(k, expand_env(v));
        }
        Ok(().serve(TokioChildProcess::new(cmd)?).await?)
    }
}

/// One remote MCP tool, presented to the engine under its namespaced name.
struct McpTool {
    /// `mcp__<server>__<tool>` — what the LLM (and the host's hooks) see.
    name: String,
    /// The name to send in `tools/call`.
    remote_name: String,
    description: String,
    parameters: Value,
    timeout: Duration,
    /// Held so the connection outlives the tool list. Shared across every tool
    /// from the same server.
    client: Arc<McpClient>,
}

#[async_trait]
impl Tool for McpTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name.clone(),
            description: self.description.clone(),
            parameters: self.parameters.clone(),
        }
    }

    async fn execute(&self, arguments: Value) -> Result<String> {
        let arguments = match arguments {
            Value::Object(map) => Some(map),
            Value::Null => None,
            other => {
                return Err(anyhow!(
                    "{}: arguments must be a JSON object, got {other}",
                    self.name
                ))
            }
        };
        // `CallToolRequestParams` is `#[non_exhaustive]` — build then set.
        let mut params = CallToolRequestParams::default();
        params.name = self.remote_name.clone().into();
        params.arguments = arguments;
        let call = self.client.peer().call_tool(params);
        let result = tokio::time::timeout(self.timeout, call)
            .await
            .map_err(|_| anyhow!("{} timed out after {:?}", self.name, self.timeout))?
            .map_err(|e| anyhow!("{} failed: {e}", self.name))?;
        render(&self.name, result)
    }
}

/// Flatten a `CallToolResult` into the string the engine feeds back to the
/// model. `isError` becomes an `Err` so it lands in the conversation as a tool
/// error (the registry renders those for the model) rather than as content the
/// model mistakes for success.
fn render(name: &str, result: CallToolResult) -> Result<String> {
    let text = result
        .content
        .iter()
        .filter_map(|c| match &c.raw {
            RawContent::Text(t) => Some(t.text.clone()),
            // ponytail: non-text content (images, embedded resources) is
            // serialized as its JSON envelope — the engine's tool contract is
            // `String`. Upgrade path is a multimodal ToolResult in core.
            other => serde_json::to_string(other).ok(),
        })
        .collect::<Vec<_>>()
        .join("\n");

    let body = if text.is_empty() {
        result
            .structured_content
            .map(|v| v.to_string())
            .unwrap_or_default()
    } else {
        text
    };

    if result.is_error.unwrap_or(false) {
        return Err(anyhow!("{name} reported an error: {body}"));
    }
    Ok(body)
}

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

/// A [`ToolProvider`] backed by MCP servers.
///
/// Connects lazily on the first turn that asks for tools, then caches the
/// resulting tool list (and the underlying connections) for the process
/// lifetime. Servers that fail to connect are logged and dropped.
pub struct McpToolProvider {
    configs: Vec<McpServerConfig>,
    tools: OnceCell<Vec<Arc<dyn Tool>>>,
}

impl std::fmt::Debug for McpToolProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpToolProvider")
            .field(
                "servers",
                &self.configs.iter().map(|c| &c.name).collect::<Vec<_>>(),
            )
            .field("connected", &self.tools.initialized())
            .finish()
    }
}

impl McpToolProvider {
    /// Build a provider over the given servers. Disabled entries are dropped
    /// here, so nothing is spawned for them.
    #[must_use]
    pub fn new(configs: Vec<McpServerConfig>) -> Self {
        Self {
            configs: configs.into_iter().filter(|c| !c.disabled).collect(),
            tools: OnceCell::new(),
        }
    }

    /// Connect every configured server and collect its tools. Called once.
    async fn connect_all(&self) -> Vec<Arc<dyn Tool>> {
        let mut out: Vec<Arc<dyn Tool>> = Vec::new();
        for cfg in &self.configs {
            match Self::tools_from(cfg).await {
                Ok(tools) => {
                    tracing::info!(server = %cfg.name, count = tools.len(), "mcp server connected");
                    out.extend(tools);
                }
                // An MCP server is a third party: degrade, never crash.
                Err(e) => {
                    tracing::warn!(server = %cfg.name, error = %e, "mcp server unavailable — its tools are absent")
                }
            }
        }
        out
    }

    async fn tools_from(cfg: &McpServerConfig) -> Result<Vec<Arc<dyn Tool>>> {
        let client = Arc::new(connect(cfg).await?);
        let timeout = Duration::from_secs(cfg.timeout_secs.unwrap_or(DEFAULT_CALL_TIMEOUT_SECS));
        let listed = client.peer().list_all_tools().await?;
        Ok(listed
            .into_iter()
            .map(|t| {
                Arc::new(McpTool {
                    name: tool_name(&cfg.name, &t.name),
                    remote_name: t.name.to_string(),
                    description: t.description.unwrap_or_default().to_string(),
                    parameters: Value::Object((*t.input_schema).clone()),
                    timeout,
                    client: Arc::clone(&client),
                }) as Arc<dyn Tool>
            })
            .collect())
    }
}

#[async_trait]
impl ToolProvider for McpToolProvider {
    async fn tools_for(&self, _ctx: &ToolProviderContext) -> Vec<Arc<dyn Tool>> {
        self.tools.get_or_init(|| self.connect_all()).await.clone()
    }
}

/// Run several providers as one — the `LocalServer` builder takes a single
/// [`ToolProvider`], so a host with both its own tools and MCP tools composes
/// them here.
pub struct ChainedToolProvider(Vec<Arc<dyn ToolProvider>>);

impl ChainedToolProvider {
    #[must_use]
    pub fn new(providers: Vec<Arc<dyn ToolProvider>>) -> Self {
        Self(providers)
    }
}

#[async_trait]
impl ToolProvider for ChainedToolProvider {
    async fn tools_for(&self, ctx: &ToolProviderContext) -> Vec<Arc<dyn Tool>> {
        let mut out = Vec::new();
        for p in &self.0 {
            out.extend(p.tools_for(ctx).await);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::handler::server::ServerHandler;
    use rmcp::model::{
        Content, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerInfo,
    };
    use rmcp::service::RequestContext;
    use rmcp::{ErrorData, RoleServer};

    // -- config ------------------------------------------------------------

    #[test]
    fn load_missing_file_is_empty() {
        let cfg = McpConfig::load(Path::new("/nope/does/not/exist/mcp.toml")).unwrap();
        assert!(cfg.servers.is_empty());
    }

    #[test]
    fn parses_the_smooth_toml_shape() {
        let cfg: McpConfig = toml::from_str(
            r#"
            [[servers]]
            name = "playwright"
            command = "npx"
            args = ["@playwright/mcp@latest"]

            [servers.env]
            FOO = "bar"

            [[servers]]
            name = "smoo"
            url = "https://mcp.smoo.ai/mcp"
            bearer_token = "${env:SMOO_TOKEN}"
            timeout_secs = 30
            "#,
        )
        .unwrap();
        assert_eq!(cfg.servers.len(), 2);
        assert_eq!(cfg.servers[0].command, "npx");
        assert_eq!(cfg.servers[0].env["FOO"], "bar");
        assert!(!cfg.servers[0].disabled);
        assert_eq!(
            cfg.servers[1].url.as_deref(),
            Some("https://mcp.smoo.ai/mcp")
        );
        assert_eq!(cfg.servers[1].timeout_secs, Some(30));
    }

    fn srv(name: &str, command: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.into(),
            command: command.into(),
            ..Default::default()
        }
    }

    #[test]
    fn project_shadows_global_by_name() {
        let merged = McpConfig::merge(
            McpConfig {
                servers: vec![srv("fs", "global-fs"), srv("gh", "global-gh")],
            },
            McpConfig {
                servers: vec![srv("fs", "project-fs"), srv("db", "project-db")],
            },
        );
        let by_name: HashMap<_, _> = merged.iter().map(|s| (s.name.clone(), s)).collect();
        assert_eq!(merged.len(), 3, "shadowing replaces, it does not duplicate");
        assert_eq!(by_name["fs"].command, "project-fs");
        assert_eq!(by_name["gh"].command, "global-gh");
        assert_eq!(by_name["db"].command, "project-db");
    }

    #[test]
    fn merge_preserves_global_order() {
        let merged = McpConfig::merge(
            McpConfig {
                servers: vec![srv("a", "a"), srv("b", "b")],
            },
            McpConfig {
                servers: vec![srv("b", "b2")],
            },
        );
        assert_eq!(
            merged.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
    }

    #[test]
    fn expand_env_substitutes_and_tolerates_junk() {
        std::env::set_var("SMOOTH_MCP_TEST_TOKEN", "sekret");
        assert_eq!(expand_env("${env:SMOOTH_MCP_TEST_TOKEN}"), "sekret");
        assert_eq!(
            expand_env("Bearer ${env:SMOOTH_MCP_TEST_TOKEN}!"),
            "Bearer sekret!"
        );
        assert_eq!(expand_env("${env:SMOOTH_MCP_TEST_UNSET_XYZ}"), "");
        assert_eq!(expand_env("${env:unterminated"), "${env:unterminated");
        assert_eq!(expand_env("plain"), "plain");
    }

    #[test]
    fn disabled_servers_are_never_connected() {
        let provider = McpToolProvider::new(vec![
            McpServerConfig {
                name: "on".into(),
                command: "true".into(),
                ..Default::default()
            },
            McpServerConfig {
                name: "off".into(),
                command: "true".into(),
                disabled: true,
                ..Default::default()
            },
        ]);
        assert_eq!(provider.configs.len(), 1);
        assert_eq!(provider.configs[0].name, "on");
    }

    // -- namespacing -------------------------------------------------------

    #[test]
    fn tool_names_are_namespaced_and_sanitized() {
        assert_eq!(tool_name("smoo", "crm_find"), "mcp__smoo__crm_find");
        assert_eq!(
            tool_name("my server", "do.thing"),
            "mcp__my_server__do_thing"
        );
        assert_eq!(tool_name("a-b", "c-d"), "mcp__a-b__c-d");
    }

    #[test]
    fn distinct_servers_do_not_collide_on_a_shared_tool_name() {
        assert_ne!(tool_name("alpha", "search"), tool_name("beta", "search"));
    }

    // -- result rendering / error mapping ----------------------------------

    #[test]
    fn render_joins_text_content() {
        let out = render(
            "t",
            CallToolResult::success(vec![Content::text("one"), Content::text("two")]),
        )
        .unwrap();
        assert_eq!(out, "one\ntwo");
    }

    #[test]
    fn render_maps_is_error_to_err() {
        let err = render(
            "mcp__s__t",
            CallToolResult::error(vec![Content::text("boom")]),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("mcp__s__t"), "{err}");
        assert!(err.contains("boom"), "{err}");
    }

    #[test]
    fn render_falls_back_to_structured_content() {
        let mut result = CallToolResult::success(vec![]);
        result.structured_content = Some(serde_json::json!({"ok": true}));
        assert_eq!(render("t", result).unwrap(), r#"{"ok":true}"#);
    }

    // -- end-to-end over the real protocol (in-process, no network) --------

    /// A minimal MCP server: one `echo` tool, plus `explode` which reports a
    /// tool-level error. Hand-rolled rather than macro-generated so the test
    /// fixture has no schemars/codegen moving parts.
    #[derive(Clone)]
    struct EchoServer;

    impl ServerHandler for EchoServer {
        fn get_info(&self) -> ServerInfo {
            let mut info = ServerInfo::default();
            info.capabilities = ServerCapabilities::builder().enable_tools().build();
            info
        }

        async fn list_tools(
            &self,
            _request: Option<PaginatedRequestParams>,
            _ctx: RequestContext<RoleServer>,
        ) -> Result<ListToolsResult, ErrorData> {
            let schema = serde_json::json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"],
            });
            let as_obj = |v: serde_json::Value| match v {
                Value::Object(m) => Arc::new(m),
                _ => unreachable!(),
            };
            Ok(ListToolsResult {
                tools: vec![
                    rmcp::model::Tool::new("echo", "Echo the input", as_obj(schema.clone())),
                    rmcp::model::Tool::new("explode", "Always errors", as_obj(schema.clone())),
                    rmcp::model::Tool::new("hang", "Never returns", as_obj(schema)),
                ],
                ..Default::default()
            })
        }

        async fn call_tool(
            &self,
            request: CallToolRequestParams,
            _ctx: RequestContext<RoleServer>,
        ) -> Result<CallToolResult, ErrorData> {
            let text = request
                .arguments
                .as_ref()
                .and_then(|a| a.get("text"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            match request.name.as_ref() {
                "echo" => Ok(CallToolResult::success(vec![Content::text(text)])),
                "explode" => Ok(CallToolResult::error(vec![Content::text("kaboom")])),
                "hang" => {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    Ok(CallToolResult::success(vec![]))
                }
                other => Err(ErrorData::invalid_params(
                    format!("no such tool: {other}"),
                    None,
                )),
            }
        }
    }

    /// Wire an [`EchoServer`] to a client over an in-memory duplex pipe and
    /// return the MCP tools exactly as the provider builds them.
    async fn echo_server_tools() -> Vec<Arc<dyn Tool>> {
        echo_server_tools_with(Duration::from_secs(5)).await
    }

    async fn echo_server_tools_with(timeout: Duration) -> Vec<Arc<dyn Tool>> {
        let (client_io, server_io) = tokio::io::duplex(4096);
        let (sr, sw) = tokio::io::split(server_io);
        tokio::spawn(async move {
            if let Ok(running) = EchoServer.serve((sr, sw)).await {
                let _ = running.waiting().await;
            }
        });
        let (cr, cw) = tokio::io::split(client_io);
        let client = Arc::new(().serve((cr, cw)).await.expect("client handshake"));

        let listed = client.peer().list_all_tools().await.expect("tools/list");
        listed
            .into_iter()
            .map(|t| {
                Arc::new(McpTool {
                    name: tool_name("fixture", &t.name),
                    remote_name: t.name.to_string(),
                    description: t.description.unwrap_or_default().to_string(),
                    parameters: Value::Object((*t.input_schema).clone()),
                    timeout,
                    client: Arc::clone(&client),
                }) as Arc<dyn Tool>
            })
            .collect()
    }

    #[tokio::test]
    async fn lists_and_calls_tools_over_the_protocol() {
        let tools = echo_server_tools().await;
        let names: Vec<_> = tools.iter().map(|t| t.schema().name).collect();
        assert_eq!(
            names,
            [
                "mcp__fixture__echo",
                "mcp__fixture__explode",
                "mcp__fixture__hang"
            ]
        );

        let echo = &tools[0];
        assert_eq!(echo.schema().description, "Echo the input");
        assert_eq!(echo.schema().parameters["type"], "object");

        let out = echo
            .execute(serde_json::json!({ "text": "hello" }))
            .await
            .unwrap();
        assert_eq!(out, "hello");
    }

    #[tokio::test]
    async fn tool_level_errors_surface_as_err() {
        let tools = echo_server_tools().await;
        let err = tools[1]
            .execute(serde_json::json!({ "text": "x" }))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("kaboom"), "{err}");
    }

    #[tokio::test]
    async fn tools_land_in_the_per_turn_registry() {
        // The whole point of going through ToolProvider: MCP tools sit in the
        // same registry as built-ins, so host ToolHooks see them.
        let mut registry = smooth_operator_core::ToolRegistry::new();
        for tool in echo_server_tools().await {
            registry.register_arc(tool);
        }
        assert!(registry.has_tool("mcp__fixture__echo"));
    }

    #[tokio::test]
    async fn non_object_arguments_are_rejected() {
        let tools = echo_server_tools().await;
        let err = tools[0]
            .execute(serde_json::json!("just a string"))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("must be a JSON object"), "{err}");
    }

    #[tokio::test]
    async fn a_hung_call_is_abandoned_at_the_timeout() {
        let tools = echo_server_tools_with(Duration::from_millis(50)).await;
        let err = tools[2]
            .execute(serde_json::json!({ "text": "x" }))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("timed out"), "{err}");
    }

    // -- graceful degradation + composition --------------------------------

    #[tokio::test]
    async fn unreachable_server_yields_no_tools_and_does_not_panic() {
        let provider = McpToolProvider::new(vec![
            McpServerConfig {
                name: "ghost".into(),
                command: "/definitely/not/a/binary".into(),
                ..Default::default()
            },
            McpServerConfig {
                name: "malformed".into(),
                ..Default::default() // neither command nor url
            },
        ]);
        assert!(provider
            .tools_for(&ToolProviderContext::default())
            .await
            .is_empty());
    }

    #[tokio::test]
    async fn chained_provider_concatenates() {
        struct Fixed(&'static str);
        #[async_trait]
        impl ToolProvider for Fixed {
            async fn tools_for(&self, _ctx: &ToolProviderContext) -> Vec<Arc<dyn Tool>> {
                struct T(&'static str);
                #[async_trait]
                impl Tool for T {
                    fn schema(&self) -> ToolSchema {
                        ToolSchema {
                            name: self.0.into(),
                            description: String::new(),
                            parameters: serde_json::json!({"type": "object"}),
                        }
                    }
                    async fn execute(&self, _a: Value) -> Result<String> {
                        Ok(String::new())
                    }
                }
                vec![Arc::new(T(self.0)) as Arc<dyn Tool>]
            }
        }

        let chained = ChainedToolProvider::new(vec![
            Arc::new(Fixed("host_tool")),
            Arc::new(McpToolProvider::new(vec![])),
            Arc::new(Fixed("other_tool")),
        ]);
        let names: Vec<_> = chained
            .tools_for(&ToolProviderContext::default())
            .await
            .iter()
            .map(|t| t.schema().name)
            .collect();
        assert_eq!(names, ["host_tool", "other_tool"]);
    }
}
