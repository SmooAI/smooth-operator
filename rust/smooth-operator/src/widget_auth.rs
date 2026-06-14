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
use std::sync::RwLock;
use std::time::{Duration, Instant};

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

/// A cache entry: the resolved policy (or `None` for a known no-policy agent) and
/// when it was fetched, for TTL expiry.
struct CacheEntry {
    value: Option<AgentWidgetAuth>,
    fetched: Instant,
}

/// HTTP-backed provider: resolves `agentId` → [`AgentWidgetAuth`] by GETting
/// `{base_url}/{agentId}` from a host's policy service, with TTL caching.
///
/// This is the **generic mechanism** a host installs instead of writing a custom
/// [`WidgetAuthProvider`]: stand up an endpoint that returns the
/// [`AgentWidgetAuth`] JSON (`{ "allowed_origins": [...], "public_key": "..." }`)
/// for an agent, point `HttpWidgetAuth` at it, and embed-auth is enforced against
/// live data. (SmooAI backs this with an api-prime route over its agent DB.)
///
/// Response handling — chosen so a flaky policy service never *silently* opens a
/// hole:
/// - **2xx** → parse + cache the policy.
/// - **404** → cache `None` (the agent legitimately has no policy; in
///   `WIDGET_AUTH_STRICT` the server then denies it).
/// - **5xx / network / malformed body** → return `None` **without caching**, so
///   the next connect retries. Combined with strict mode this fails closed; in
///   permissive mode enforcement is off anyway.
///
/// Cached results (incl. 404s) are reused for `ttl` (default 60s) so a busy embed
/// doesn't hammer the policy service on every WebSocket connect.
pub struct HttpWidgetAuth {
    client: reqwest::Client,
    /// Policy endpoint base (no trailing slash); the agent id is appended as a
    /// single percent-encoded path segment.
    base_url: String,
    /// Optional bearer token sent to the policy service (e.g. an M2M token).
    bearer: Option<String>,
    ttl: Duration,
    cache: RwLock<HashMap<String, CacheEntry>>,
}

impl HttpWidgetAuth {
    /// Build a provider that resolves policies from `base_url` (e.g.
    /// `https://api.smoo.ai/internal/widget-auth`). Uses a client with a 5s
    /// timeout so a hung policy service can't stall widget connects.
    #[must_use]
    pub fn new(base_url: impl Into<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap_or_default();
        Self::with_client(base_url, client)
    }

    /// Build with a caller-supplied [`reqwest::Client`] (to share a pool / set
    /// custom timeouts or TLS).
    #[must_use]
    pub fn with_client(base_url: impl Into<String>, client: reqwest::Client) -> Self {
        Self {
            client,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            bearer: None,
            ttl: Duration::from_secs(60),
            cache: RwLock::new(HashMap::new()),
        }
    }

    /// Send `Authorization: Bearer <token>` to the policy service (builder).
    #[must_use]
    pub fn with_bearer(mut self, token: impl Into<String>) -> Self {
        self.bearer = Some(token.into());
        self
    }

    /// Override the cache TTL (builder). Default 60s.
    #[must_use]
    pub fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    /// A live (non-expired) cached result for `agent_id`, if any. Outer `None` =
    /// not cached / expired; inner `Option` = the cached policy-or-no-policy.
    fn cached(&self, agent_id: &str) -> Option<Option<AgentWidgetAuth>> {
        let cache = self.cache.read().ok()?;
        let entry = cache.get(agent_id)?;
        if entry.fetched.elapsed() < self.ttl {
            Some(entry.value.clone())
        } else {
            None
        }
    }

    /// Cache a definitive result (a 2xx policy or a 404 no-policy).
    fn store(&self, agent_id: &str, value: Option<AgentWidgetAuth>) {
        if let Ok(mut cache) = self.cache.write() {
            cache.insert(
                agent_id.to_string(),
                CacheEntry {
                    value,
                    fetched: Instant::now(),
                },
            );
        }
    }
}

#[async_trait]
impl WidgetAuthProvider for HttpWidgetAuth {
    async fn agent_widget_auth(&self, agent_id: &str) -> Option<AgentWidgetAuth> {
        if let Some(cached) = self.cached(agent_id) {
            return cached;
        }

        // Build the URL by pushing the agent id as one percent-encoded segment,
        // so an id can't manipulate the path.
        let mut url = match reqwest::Url::parse(&self.base_url) {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!(error = %e, base_url = %self.base_url, "widget-auth: invalid base_url");
                return None;
            }
        };
        match url.path_segments_mut() {
            Ok(mut segs) => {
                segs.push(agent_id);
            }
            Err(()) => {
                tracing::warn!(base_url = %self.base_url, "widget-auth: base_url cannot be a base");
                return None;
            }
        }

        let mut req = self.client.get(url);
        if let Some(bearer) = &self.bearer {
            req = req.bearer_auth(bearer);
        }

        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                // Transient — do NOT cache, so the next connect retries.
                tracing::warn!(error = %e, agent_id, "widget-auth: policy fetch failed");
                return None;
            }
        };

        let status = resp.status();
        if status.is_success() {
            match resp.json::<AgentWidgetAuth>().await {
                Ok(policy) => {
                    let value = Some(policy);
                    self.store(agent_id, value.clone());
                    value
                }
                Err(e) => {
                    // Malformed body (deploy skew?) — don't cache; retry next time.
                    tracing::warn!(error = %e, agent_id, "widget-auth: malformed policy body");
                    None
                }
            }
        } else if status == reqwest::StatusCode::NOT_FOUND {
            // Legitimate "this agent has no policy" — cache it.
            self.store(agent_id, None);
            None
        } else {
            // 5xx etc. — don't cache; fail open here, which strict mode turns
            // into a deny.
            tracing::warn!(%status, agent_id, "widget-auth: policy service error");
            None
        }
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

    #[tokio::test]
    async fn http_provider_fetches_then_serves_from_cache() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/agent-9"))
            .and(header("authorization", "Bearer m2m-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "allowed_origins": ["https://app.smoo.ai"],
                "public_key": "secret"
            })))
            .expect(1) // second call must be served from cache, not the server
            .mount(&server)
            .await;

        let provider = HttpWidgetAuth::new(server.uri()).with_bearer("m2m-token");

        let first = provider.agent_widget_auth("agent-9").await.expect("policy");
        assert_eq!(
            first.allowed_origins,
            vec!["https://app.smoo.ai".to_string()]
        );
        assert_eq!(first.public_key.as_deref(), Some("secret"));

        // Cache hit — no second upstream request (verified by `.expect(1)` on drop).
        let second = provider.agent_widget_auth("agent-9").await.expect("cached");
        assert_eq!(second.public_key.as_deref(), Some("secret"));
    }

    #[tokio::test]
    async fn http_provider_404_is_none_and_cached() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ghost"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1) // a known no-policy result is cached too
            .mount(&server)
            .await;

        let provider = HttpWidgetAuth::new(server.uri());
        assert!(provider.agent_widget_auth("ghost").await.is_none());
        assert!(provider.agent_widget_auth("ghost").await.is_none()); // cached
    }

    #[tokio::test]
    async fn http_provider_server_error_is_none_and_not_cached() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/flaky"))
            .respond_with(ResponseTemplate::new(500))
            .expect(2) // NOT cached on error → the next call retries upstream
            .mount(&server)
            .await;

        let provider = HttpWidgetAuth::new(server.uri());
        assert!(provider.agent_widget_auth("flaky").await.is_none());
        assert!(provider.agent_widget_auth("flaky").await.is_none()); // retried, not cached
    }
}
