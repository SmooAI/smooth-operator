//! SEP extension hosting for the operator server.
//!
//! Wires the engine's [`ExtensionHost`](smooth_operator_core::extension::ExtensionHost)
//! into a turn so a server-side agent can host extensions: discover
//! `extension.toml` extensions, spawn them as JSON-RPC/ndjson subprocesses,
//! register their tools into the turn's [`ToolRegistry`], and run their hooks in
//! the agent loop. The host is attached in
//! [`run_streaming_turn`](crate::runner::run_streaming_turn) via
//! [`Agent::with_extension_host`](smooth_operator_core::Agent::with_extension_host).
//!
//! ## Trust — default deny
//! The server has no interactive trust prompt (a multi-session daemon can't stop
//! to ask a human). `SMOOTH_EXTENSIONS_ALLOW` (comma-separated extension names)
//! IS the trust decision: empty (the default) ⇒ **no extension is ever spawned**
//! and the host is never built, so behavior is byte-for-byte unchanged.
//!
//! ## `ui/confirm` → the existing confirmation frame
//! [`ConfirmUiProvider`] projects an extension's `ui/confirm` onto the operator
//! protocol's `write_confirmation_required` / `confirm_tool_action` frames — the
//! same out-of-band bridge the native write-tool `ConfirmationHook` uses
//! (`runner::spawn_confirmation_bridge`): register a resumable
//! [`HumanResponse`](smooth_operator_core::HumanResponse) sender under the
//! session, emit the frame, and park the extension's request until the client
//! answers with `confirm_tool_action`. Every other `ui/*` degrades headless
//! (interactive → `{cancelled}`, render-only → `{}`); we advertise only the
//! `confirm` capability at handshake so a well-behaved extension gates the rest
//! off via `hasUI`.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use smooth_operator_core::extension::manifest::{default_global_dir, project_dir};
use smooth_operator_core::extension::protocol::{HostInfo, RpcError, WorkspaceInfo};
use smooth_operator_core::extension::{discover, DiscoveredExtension, ExtensionHost, HostDelegate};
use smooth_operator_core::HumanResponse;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

use crate::runner::{ClearConfirmation, RegisterConfirmation};
use crate::state::AppState;

/// Frontend `mode` announced to extensions at handshake. The five servers front
/// the chat-widget, whose confirm/select land as chat-native button frames.
const UI_MODE: &str = "widget";

/// How long a parked `ui/confirm` waits for the client's `confirm_tool_action`
/// before the bridge resolves it as cancelled. Matches the native write-tool
/// confirmation window so both HITL paths behave the same to a client.
const UI_CONFIRM_TIMEOUT: Duration = Duration::from_secs(300);

/// A per-turn extension host plus the teardown hook the runner needs. Held on the
/// [`TurnRequest`](crate::runner::TurnRequest); the runner registers the host's
/// tools, attaches it to the agent, and calls [`clear`](Self::clear) at turn end
/// to drop any confirmation registration the turn left parked.
pub struct ExtensionTurn {
    /// The loaded host (shared with the agent via `with_extension_host`).
    pub host: Arc<ExtensionHost>,
    /// The session this turn belongs to — the key the confirmation registry uses.
    pub session_id: String,
    /// Clears any `ui/confirm` responder still registered for the session when the
    /// turn ends (typically `AppState::clear_confirmation`).
    pub clear: ClearConfirmation,
}

/// The [`HostDelegate`] that bridges `ui/confirm` onto the confirmation frame and
/// degrades every other `ui/*` headless. Bound to ONE turn (its sink, request id,
/// session), which is why the host is built per turn — a shared host could not
/// route a `ui/*` back to the right session's socket.
struct ConfirmUiProvider {
    /// The turn's protocol sink — where the `write_confirmation_required` frame goes.
    sink: UnboundedSender<Value>,
    /// The turn's protocol request id (streaming correlation on the frame).
    request_id: String,
    /// The session the confirmation is registered under.
    session_id: String,
    /// Registers the parked responder so an inbound `confirm_tool_action` resumes it.
    register: RegisterConfirmation,
}

/// Whether an extension may load this turn: it must be in the server allowlist
/// AND pass the per-agent gate. The per-agent gate (SMOODEV-2259):
/// - `None` ⇒ no per-agent config resolved (bare/standalone operator) ⇒
///   unrestricted, preserving the pre-per-agent behavior (server allowlist only);
/// - `Some(ids)` ⇒ a per-agent config WAS resolved ⇒ the extension must ALSO be in
///   `ids`. `Some(&[])` (a resolved agent that enables nothing) therefore admits
///   nothing = **fail-closed**. Extensions can intercept & mutate tool calls, so a
///   public agent must never silently inherit one from the server allowlist.
fn extension_allowed(name: &str, allow: &[String], enabled_extensions: Option<&[String]>) -> bool {
    allow.iter().any(|a| a == name)
        && enabled_extensions.is_none_or(|ids| ids.iter().any(|id| id == name))
}

/// Parse `SMOOTH_EXTENSIONS_ALLOW` into a set of allowed extension names
/// (comma-separated, trimmed, empties dropped). Absent/blank ⇒ empty ⇒ deny all.
fn parse_allowlist(raw: Option<&str>) -> Vec<String> {
    raw.unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

#[async_trait]
impl HostDelegate for ConfirmUiProvider {
    async fn ui_request(&self, ext: &str, params: Value) -> Result<Value, RpcError> {
        let kind = params
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match kind {
            "confirm" => {
                let prompt = params
                    .get("prompt")
                    .and_then(Value::as_str)
                    .unwrap_or("Confirm this action?");
                // Register a fresh responder for this session so the next inbound
                // `confirm_tool_action` resumes THIS request, then emit the frame
                // and park until the human answers (or we time out). The inbound
                // handler `take`s the responder, so one confirm resolves one park.
                let (tx, mut rx) = unbounded_channel::<HumanResponse>();
                (self.register)(&self.session_id, tx);
                let _ = self.sink.send(crate::protocol::write_confirmation_required(
                    &self.request_id,
                    ext,
                    prompt,
                ));
                match tokio::time::timeout(UI_CONFIRM_TIMEOUT, rx.recv()).await {
                    Ok(Some(HumanResponse::Approved)) => Ok(json!({ "confirmed": true })),
                    Ok(Some(HumanResponse::Denied { .. })) => Ok(json!({ "confirmed": false })),
                    // Denied via Input/Timeout, a closed channel (turn ended), or
                    // our own timeout all read as a dismissed dialog.
                    _ => Ok(json!({ "cancelled": true })),
                }
            }
            // Render-only kinds: accept and drop — there's no chat frame for them
            // and nothing to await.
            "notify" | "set_status" | "set_widget" | "set_title" => Ok(json!({})),
            // select/input need an answer we can't source from a confirm button.
            _ => Ok(json!({ "cancelled": true })),
        }
    }
}

/// Discover, trust-gate (allowlist), and load the per-turn extension host for a
/// session's turn. Returns `None` — the host is never built, zero overhead —
/// when the allowlist is empty (default deny) or no allowed extension loads.
///
// ponytail: per-TURN spawn. One subprocess set per turn *when extensions are
// configured*; correct for multi-session routing (the ui/confirm delegate is
// turn-scoped) and free when unconfigured (empty allowlist → early `None`).
// Upgrade path: cache a per-connection host if turn latency with extensions
// installed ever matters.
pub async fn build_extension_host(
    state: &AppState,
    session_id: &str,
    request_id: &str,
    sink: UnboundedSender<Value>,
    enabled_extensions: Option<&[String]>,
) -> Option<ExtensionTurn> {
    // Trust = a default-deny env allowlist (the server has no interactive prompt).
    // `SMOOTH_EXTENSIONS_ALLOW` comma-separated names; empty/unset ⇒ never build.
    let allow = parse_allowlist(std::env::var("SMOOTH_EXTENSIONS_ALLOW").ok().as_deref());
    if allow.is_empty() {
        return None; // default deny — never spawn anything
    }

    // `SMOOTH_EXTENSIONS_DIR` overrides the discovery dir; else the engine default
    // (`$SMOOTH_HOME/extensions` or `~/.smooth/extensions`).
    let global = std::env::var("SMOOTH_EXTENSIONS_DIR")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(default_global_dir);
    // The server has no per-session workspace; project-scoped discovery keys off
    // the process cwd's `.smooth/extensions`. Usually absent → global only.
    let project = std::env::current_dir().ok().map(|d| project_dir(&d));
    let (discovered, disc_failures) = discover(global.as_deref(), project.as_deref());
    for (src, err) in &disc_failures {
        tracing::warn!(%src, %err, "sep: extension manifest failed to parse");
    }

    let allowed: Vec<DiscoveredExtension> = discovered
        .into_iter()
        .filter(|ext| {
            let ok = extension_allowed(&ext.manifest.name, &allow, enabled_extensions);
            if !ok {
                tracing::info!(name = %ext.manifest.name, "sep: skipping extension — not in SMOOTH_EXTENSIONS_ALLOW ∩ per-agent enabled extensions");
            }
            ok
        })
        .collect();
    if allowed.is_empty() {
        return None;
    }

    let host_info = HostInfo {
        name: "smooth-operator-server".into(),
        version: env!("CARGO_PKG_VERSION").into(),
    };
    // Allowlisted ⇒ trusted (the allowlist is the trust decision); project-scoped
    // extensions load because `trusted` is true.
    let workspace = WorkspaceInfo {
        root: std::env::current_dir()
            .map(|d| d.to_string_lossy().into_owned())
            .unwrap_or_default(),
        trusted: true,
    };
    let register: RegisterConfirmation = {
        let state = state.clone();
        Arc::new(move |sid: &str, responder| state.register_confirmation(sid, responder))
    };
    let clear: ClearConfirmation = {
        let state = state.clone();
        Arc::new(move |sid: &str| state.clear_confirmation(sid))
    };
    let delegate = Arc::new(ConfirmUiProvider {
        sink,
        request_id: request_id.to_string(),
        session_id: session_id.to_string(),
        register,
    });

    let (host, load_failures) = ExtensionHost::load(
        allowed,
        host_info,
        workspace,
        UI_MODE,
        vec!["confirm".to_string()],
        delegate,
    )
    .await;
    for (name, err) in &load_failures {
        tracing::warn!(%name, %err, "sep: extension failed to load");
    }
    if host.is_empty() {
        return None;
    }
    tracing::info!(count = host.len(), extensions = ?host.names(), "sep: attached extension host to the turn");
    Some(ExtensionTurn {
        host: Arc::new(host),
        session_id: session_id.to_string(),
        clear,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A minimal stand-in for the AppState confirmation registry: the closure
    /// stores the parked responder in a slot a test can `take` and answer.
    fn test_register(
        slot: Arc<Mutex<Option<UnboundedSender<HumanResponse>>>>,
    ) -> RegisterConfirmation {
        Arc::new(move |_sid: &str, responder| {
            *slot.lock().unwrap() = Some(responder);
        })
    }

    fn provider(
        sink: UnboundedSender<Value>,
        slot: Arc<Mutex<Option<UnboundedSender<HumanResponse>>>>,
    ) -> ConfirmUiProvider {
        ConfirmUiProvider {
            sink,
            request_id: "req-1".into(),
            session_id: "sess-1".into(),
            register: test_register(slot),
        }
    }

    #[test]
    fn allowlist_parses_csv_and_denies_by_default() {
        assert!(parse_allowlist(None).is_empty(), "unset ⇒ deny all");
        assert!(parse_allowlist(Some("")).is_empty(), "blank ⇒ deny all");
        assert!(
            parse_allowlist(Some("  , ,")).is_empty(),
            "only separators ⇒ deny all"
        );
        assert_eq!(parse_allowlist(Some("todo")), vec!["todo".to_string()]);
        assert_eq!(
            parse_allowlist(Some(" todo , gate ")),
            vec!["todo".to_string(), "gate".to_string()]
        );
    }

    #[test]
    fn extension_allowed_intersects_server_allowlist_with_per_agent_ids() {
        let allow = vec!["a".to_string(), "b".to_string()];

        // No per-agent config resolved (bare/standalone operator) ⇒ unrestricted:
        // the server allowlist alone decides (backward-compatible).
        assert!(extension_allowed("a", &allow, None));
        assert!(extension_allowed("b", &allow, None));
        assert!(
            !extension_allowed("c", &allow, None),
            "not in server allowlist"
        );

        // A resolved agent that enables only `a`: server allowlist ∩ {a} = {a}.
        let only_a = vec!["a".to_string()];
        assert!(extension_allowed("a", &allow, Some(&only_a)));
        assert!(
            !extension_allowed("b", &allow, Some(&only_a)),
            "b is allowed by server but NOT enabled per-agent"
        );

        // A resolved agent that enables NOTHING (empty) ⇒ fail-closed: nothing
        // loads even though the server allowlist is non-empty.
        let none_enabled: Vec<String> = vec![];
        assert!(!extension_allowed("a", &allow, Some(&none_enabled)));
        assert!(!extension_allowed("b", &allow, Some(&none_enabled)));

        // A per-agent id NOT in the server allowlist still can't load (intersection).
        let wants_c = vec!["c".to_string()];
        assert!(!extension_allowed("c", &allow, Some(&wants_c)));
    }

    #[tokio::test]
    async fn confirm_emits_frame_and_resolves_on_approval() {
        let (sink_tx, mut sink_rx) = unbounded_channel::<Value>();
        let slot = Arc::new(Mutex::new(None));
        let p = provider(sink_tx, slot.clone());

        let params = json!({ "kind": "confirm", "prompt": "Delete file?" });
        let fut = tokio::spawn(async move { p.ui_request("todo", params).await });

        // The bridge emitted a confirmation frame carrying the ext name as toolId.
        let frame = sink_rx.recv().await.expect("frame");
        assert_eq!(frame["type"], "write_confirmation_required");
        assert_eq!(frame["data"]["data"]["toolId"], "todo");
        assert_eq!(frame["data"]["data"]["actionDescription"], "Delete file?");

        // Simulate the inbound confirm_tool_action approving the action.
        let responder = slot.lock().unwrap().take().expect("responder registered");
        responder.send(HumanResponse::Approved).unwrap();

        let result = fut.await.unwrap().unwrap();
        assert_eq!(result, json!({ "confirmed": true }));
    }

    #[tokio::test]
    async fn confirm_resolves_false_on_denial() {
        let (sink_tx, mut sink_rx) = unbounded_channel::<Value>();
        let slot = Arc::new(Mutex::new(None));
        let p = provider(sink_tx, slot.clone());

        let params = json!({ "kind": "confirm", "prompt": "Proceed?" });
        let fut = tokio::spawn(async move { p.ui_request("gate", params).await });

        let _ = sink_rx.recv().await.expect("frame");
        let responder = slot.lock().unwrap().take().expect("responder");
        responder
            .send(HumanResponse::Denied {
                reason: "no".into(),
            })
            .unwrap();

        assert_eq!(fut.await.unwrap().unwrap(), json!({ "confirmed": false }));
    }

    #[tokio::test]
    async fn confirm_cancels_when_turn_ends() {
        let (sink_tx, mut sink_rx) = unbounded_channel::<Value>();
        let slot = Arc::new(Mutex::new(None));
        let p = provider(sink_tx, slot.clone());

        let params = json!({ "kind": "confirm", "prompt": "Go?" });
        let fut = tokio::spawn(async move { p.ui_request("x", params).await });

        let _ = sink_rx.recv().await.expect("frame");
        // Drop the responder without answering — the parked turn ended.
        drop(slot.lock().unwrap().take());

        assert_eq!(fut.await.unwrap().unwrap(), json!({ "cancelled": true }));
    }

    #[tokio::test]
    async fn render_only_kinds_accept_and_drop() {
        let (sink_tx, _sink_rx) = unbounded_channel::<Value>();
        let slot = Arc::new(Mutex::new(None));
        let p = provider(sink_tx, slot);

        for kind in ["notify", "set_status", "set_widget", "set_title"] {
            let params =
                json!({ "kind": kind, "message": "hi", "status": "s", "widget": {}, "title": "t" });
            assert_eq!(
                p.ui_request("x", params).await.unwrap(),
                json!({}),
                "kind {kind}"
            );
        }
    }

    #[tokio::test]
    async fn unsupported_interactive_kinds_cancel() {
        let (sink_tx, _sink_rx) = unbounded_channel::<Value>();
        let slot = Arc::new(Mutex::new(None));
        let p = provider(sink_tx, slot);

        for kind in ["select", "input"] {
            let params = json!({ "kind": kind, "prompt": "?", "options": ["a"] });
            assert_eq!(
                p.ui_request("x", params).await.unwrap(),
                json!({ "cancelled": true }),
                "kind {kind}"
            );
        }
    }
}
