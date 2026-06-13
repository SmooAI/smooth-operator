//! Embeddable-widget auth: an origin allowlist + public-key `authContext`
//! verification for browser-embedded chat widgets (`<smooth-agent-chat>`).
//!
//! Browser widgets connect from arbitrary customer sites, so an agent that's
//! embeddable needs two protections the bearer-token path doesn't give:
//!
//! 1. **Origin allowlist** — only the sites a customer registered may embed and
//!    drive their agent (mirrors a CORS/referrer allowlist, enforced server-side
//!    on the WebSocket `Origin` header captured at connect).
//! 2. **Public-key `authContext`** — a host page can pre-authenticate a known
//!    user by HMAC-signing `{userId}:{timestamp}` with the agent's public key;
//!    the server verifies it (replay-protected) so the turn can skip OTP.
//!
//! This module is the **hook**: the public server defines the
//! [`WidgetAuthProvider`] trait + the enforcement primitives ([`origin_allowed`],
//! [`verify_auth_context`]); the host application plugs in a concrete provider
//! (e.g. backed by its agent database) that maps an `agentId` to its
//! [`AgentWidgetAuth`] policy. The bundled [`PermissiveWidgetAuth`] returns no
//! policy for any agent, so a standalone OSS server enforces nothing until a
//! real provider is installed (see `WIDGET_AUTH_STRICT` on the server for
//! fail-closed behavior on unknown agents).

use std::collections::HashMap;

use async_trait::async_trait;
use hmac::{Hmac, Mac};
use sha2::Sha256;

/// The embed-auth policy for one agent.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct AgentWidgetAuth {
    /// Origins permitted to embed this agent. Each entry is an exact origin
    /// (`https://app.example.com`), a host wildcard (`https://*.smoo.ai`), or
    /// `*` (any). An **empty** list means *no origin is allowed* (deny all) —
    /// configure at least one entry to permit embedding.
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    /// Shared secret used to verify a pre-auth `authContext` HMAC. `None` means
    /// the agent does not support `authContext` (any supplied one is rejected).
    #[serde(default)]
    pub public_key: Option<String>,
}

/// Hook for resolving an agent's [`AgentWidgetAuth`] policy.
///
/// Implemented by the host application (commonly backed by its agent DB/API).
/// Returning `None` means "no policy for this agent" — the server treats that as
/// allow in permissive mode, or deny in strict mode (`WIDGET_AUTH_STRICT`).
#[async_trait]
pub trait WidgetAuthProvider: Send + Sync {
    /// The embed-auth policy for `agent_id`, or `None` if the agent has none /
    /// is unknown.
    async fn agent_widget_auth(&self, agent_id: &str) -> Option<AgentWidgetAuth>;
}

/// Default provider: no policy for any agent → enforcement is off. Keeps the OSS
/// server's `/ws` path open until a real [`WidgetAuthProvider`] is installed.
#[derive(Debug, Default)]
pub struct PermissiveWidgetAuth;

#[async_trait]
impl WidgetAuthProvider for PermissiveWidgetAuth {
    async fn agent_widget_auth(&self, _agent_id: &str) -> Option<AgentWidgetAuth> {
        None
    }
}

/// Static map provider (`agentId` → policy). Lets a server enforce without a
/// database, and gives hosts a simple wiring option (load from a JSON file/env).
#[derive(Debug, Default)]
pub struct StaticWidgetAuth {
    rows: HashMap<String, AgentWidgetAuth>,
}

impl StaticWidgetAuth {
    /// Build from an in-memory map.
    #[must_use]
    pub fn new(rows: HashMap<String, AgentWidgetAuth>) -> Self {
        Self { rows }
    }

    /// Parse a JSON object of `{ "<agentId>": { "allowed_origins": [...],
    /// "public_key": "..." }, ... }`.
    ///
    /// # Errors
    /// Returns an error if `json` is not a valid map of the expected shape.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        let rows: HashMap<String, AgentWidgetAuth> = serde_json::from_str(json)?;
        Ok(Self { rows })
    }
}

#[async_trait]
impl WidgetAuthProvider for StaticWidgetAuth {
    async fn agent_widget_auth(&self, agent_id: &str) -> Option<AgentWidgetAuth> {
        self.rows.get(agent_id).cloned()
    }
}

/// Whether `origin` is permitted by `allowed`.
///
/// An empty `allowed` permits nothing. Each pattern is matched as:
/// - `*` → any origin,
/// - an exact match (`https://app.example.com`),
/// - a host wildcard `scheme://*.suffix` → the origin's scheme must match and
///   its host must equal `suffix` or end with `.suffix`
///   (`https://*.smoo.ai` matches `https://app.smoo.ai` and `https://smoo.ai`).
#[must_use]
pub fn origin_allowed(allowed: &[String], origin: &str) -> bool {
    allowed
        .iter()
        .any(|pattern| origin_matches(pattern, origin))
}

fn origin_matches(pattern: &str, origin: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if pattern == origin {
        return true;
    }
    // Host wildcard: scheme://*.suffix
    let (Some((p_scheme, p_host)), Some((o_scheme, o_host))) =
        (pattern.split_once("://"), origin.split_once("://"))
    else {
        return false;
    };
    if p_scheme != o_scheme {
        return false;
    }
    if let Some(suffix) = p_host.strip_prefix("*.") {
        return o_host == suffix || o_host.ends_with(&format!(".{suffix}"));
    }
    false
}

/// Verify a pre-auth `authContext`: an HMAC-SHA256 over `"{user_id}:{timestamp}"`
/// keyed by `public_key`, encoded as lowercase hex in `signature_hex`, signed no
/// more than `max_age_secs` away from `now_unix` (replay protection).
///
/// Returns `false` (never panics) on any malformed input, a stale/future
/// timestamp, or a signature mismatch. The comparison is constant-time
/// (`Mac::verify_slice`).
#[must_use]
pub fn verify_auth_context(
    public_key: &str,
    user_id: &str,
    signature_hex: &str,
    timestamp: i64,
    now_unix: i64,
    max_age_secs: i64,
) -> bool {
    // Replay window: reject timestamps too far in the past or future.
    if (now_unix - timestamp).abs() > max_age_secs {
        return false;
    }
    let Ok(sig) = hex::decode(signature_hex) else {
        return false;
    };
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(public_key.as_bytes()) else {
        return false;
    };
    mac.update(format!("{user_id}:{timestamp}").as_bytes());
    mac.verify_slice(&sig).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_exact_and_wildcard() {
        let allow = vec![
            "https://app.example.com".to_string(),
            "https://*.smoo.ai".to_string(),
        ];
        assert!(origin_allowed(&allow, "https://app.example.com"));
        assert!(origin_allowed(&allow, "https://dash.smoo.ai"));
        assert!(origin_allowed(&allow, "https://smoo.ai"));
        assert!(!origin_allowed(&allow, "https://evil.com"));
        // Scheme must match.
        assert!(!origin_allowed(&allow, "http://dash.smoo.ai"));
        // Not a sub-suffix match.
        assert!(!origin_allowed(&allow, "https://notsmoo.ai"));
    }

    #[test]
    fn origin_star_allows_all_but_empty_denies() {
        assert!(origin_allowed(&["*".to_string()], "https://anything.test"));
        assert!(!origin_allowed(&[], "https://anything.test"));
    }

    fn sign(key: &str, user: &str, ts: i64) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(key.as_bytes()).unwrap();
        mac.update(format!("{user}:{ts}").as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    #[test]
    fn auth_context_valid_and_invalid() {
        let key = "super-secret-public-key";
        let now = 1_000_000;
        let good = sign(key, "user-123", now);
        assert!(verify_auth_context(key, "user-123", &good, now, now, 60));
        // Within the window but slightly old.
        assert!(verify_auth_context(
            key,
            "user-123",
            &sign(key, "user-123", now - 30),
            now - 30,
            now,
            60
        ));
        // Wrong key.
        assert!(!verify_auth_context(
            "other-key",
            "user-123",
            &good,
            now,
            now,
            60
        ));
        // Tampered user.
        assert!(!verify_auth_context(key, "user-999", &good, now, now, 60));
        // Stale (outside replay window).
        assert!(!verify_auth_context(
            key,
            "user-123",
            &sign(key, "user-123", now - 600),
            now - 600,
            now,
            60
        ));
        // Garbage signature.
        assert!(!verify_auth_context(
            key, "user-123", "not-hex", now, now, 60
        ));
    }

    #[tokio::test]
    async fn static_provider_resolves_known_agents() {
        let json =
            r#"{ "agent-1": { "allowed_origins": ["https://*.smoo.ai"], "public_key": "k" } }"#;
        let p = StaticWidgetAuth::from_json(json).unwrap();
        let a = p.agent_widget_auth("agent-1").await.unwrap();
        assert_eq!(a.allowed_origins, vec!["https://*.smoo.ai".to_string()]);
        assert_eq!(a.public_key.as_deref(), Some("k"));
        assert!(p.agent_widget_auth("unknown").await.is_none());
    }

    #[tokio::test]
    async fn permissive_provider_returns_none() {
        assert!(PermissiveWidgetAuth
            .agent_widget_auth("anything")
            .await
            .is_none());
    }
}
