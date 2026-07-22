//! Authentication + role-based access control (Phase 12).
//!
//! This is the auth seam the management console (Next.js, increment 2) consumes
//! through the admin HTTP API. It defines:
//!
//! - [`Role`] — `Admin >= Curator >= Basic`, a total order so a route can gate
//!   on a *minimum* role.
//! - [`Principal`] — the authenticated identity a request runs as (`user_id`,
//!   `org_id`, `role`, optional `display_name`). Org-scoping everything to this
//!   `org_id` is how the admin API stays multi-tenant-safe.
//! - [`AuthVerifier`] — the one seam that turns a bearer token into a
//!   [`Principal`]. Three impls cover the deployment shapes:
//!   - [`JwtVerifier`] — **BYO** path: validates a JWT issued by the customer's
//!     own IdP. SST OpenAuth (`@openauthjs/openauth` + `sst.aws.Auth`) issues
//!     exactly these. HS256 (shared secret) and RS256 (public key) supported.
//!   - [`SmooIdentityVerifier`] — **hosted** path: validates a Smoo-issued JWT
//!     keyed to Smoo's issuer/audience (lom.smoo.ai wires Smoo's identity). The
//!     live token-introspection variant is documented + stubbed (it needs a
//!     network call to the auth server's `/introspect`).
//!   - [`NoAuthVerifier`] — **dev only**: returns a fixed `Admin` principal.
//!     Reachable *only* when `AUTH_MODE=none` is set explicitly, so it can never
//!     be the silent production default.
//!
//! ## Secure-by-default
//!
//! [`AuthConfig::from_env`] selects the verifier from `AUTH_MODE`
//! (`jwt` | `smoo` | `none`). The **default is `jwt`** — and if `jwt`/`smoo` is
//! selected without a configured key the constructor returns an
//! [`AuthError::Misconfigured`] error rather than silently falling back to
//! no-auth. Only an explicit `AUTH_MODE=none` yields [`NoAuthVerifier`].
//!
//! ## Relationship to [`AccessContext`](crate::access_control::AccessContext)
//!
//! RBAC ([`Role`]) gates *which admin operations* a principal may perform;
//! [`AccessContext`](crate::access_control::AccessContext) gates *which
//! documents* a retrieval may return. A [`Principal`] maps to an
//! [`AccessContext`] via [`Principal::access_context`] so the same identity
//! drives both layers.

use std::collections::HashSet;
use std::fmt;
use std::str::FromStr;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use jsonwebtoken::jwk::{Jwk, JwkSet};
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::{Deserialize, Serialize};

use crate::access_control::AccessContext;

/// A role in the org's RBAC model. Ordered so `Admin > Curator > Basic`, which
/// lets a route gate on a *minimum* role with `principal.role >= min`.
///
/// - **Admin** — full org-wide read of chat history, indexing, document sets,
///   and (future) write/config.
/// - **Curator** — org-wide read of chat history + curation surfaces (indexing,
///   document sets); the knowledge-curation persona.
/// - **Basic** — an end user: may see only their *own* conversations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Lowest privilege — sees only their own data.
    Basic,
    /// Curation persona — org-wide read of curation surfaces.
    Curator,
    /// Highest privilege — full org-wide access.
    Admin,
}

impl Role {
    /// Parse a role from a claim string (case-insensitive). Unknown / absent
    /// values are an error so a token can never silently downgrade *or* upgrade.
    ///
    /// # Errors
    /// Returns [`AuthError::MissingRole`] when the value isn't a known role.
    pub fn parse(value: &str) -> Result<Self, AuthError> {
        match value.trim().to_ascii_lowercase().as_str() {
            "admin" => Ok(Role::Admin),
            "curator" => Ok(Role::Curator),
            "basic" | "user" => Ok(Role::Basic),
            other => Err(AuthError::MissingRole(format!("unknown role '{other}'"))),
        }
    }

    /// The wire/string form of this role.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::Curator => "curator",
            Role::Basic => "basic",
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The authenticated identity a request runs as. Everything the admin API reads
/// is scoped to [`org_id`](Principal::org_id); [`role`](Principal::role) gates
/// which operations are allowed and whether reads are org-wide or self-only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Principal {
    /// Stable user id (the JWT `sub`).
    pub user_id: String,
    /// The organization this principal belongs to (the JWT `org` / `org_id`).
    /// Every admin read is filtered to this org.
    pub org_id: String,
    /// The principal's role in the org.
    pub role: Role,
    /// Optional human-readable name (the JWT `name`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// The principal's email (the JWT `email` claim), when the token carries
    /// one. This is the **per-user scope key** for conversation reads
    /// (`list_conversations`, resume, `get_conversation_messages`): it comes
    /// from the *verified* token and is never read from client-supplied frame
    /// fields, which a caller can set to anyone's address. Absent ⇒ the
    /// connection carries no user identity and conversation reads fail closed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// The groups the principal belongs to (the JWT `groups` claim). These are
    /// the entitlements the document-level ACL layer matches against: a
    /// document scoped to group `github:owner/repo` is readable only by a
    /// principal carrying that group. Empty when the token has no `groups`
    /// claim (the principal then sees only org-public + user-scoped docs).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<String>,
}

impl Principal {
    /// Construct a principal (mostly for tests + the no-auth path).
    #[must_use]
    pub fn new(
        user_id: impl Into<String>,
        org_id: impl Into<String>,
        role: Role,
        display_name: Option<String>,
    ) -> Self {
        Self {
            user_id: user_id.into(),
            org_id: org_id.into(),
            role,
            display_name,
            email: None,
            groups: Vec::new(),
        }
    }

    /// Attach the principal's email (builder) — the per-user conversation scope
    /// key. Blank/whitespace-only input is treated as absent (fail closed).
    #[must_use]
    pub fn with_email(mut self, email: impl Into<String>) -> Self {
        let email = email.into();
        self.email = (!email.trim().is_empty()).then_some(email);
        self
    }

    /// Attach group memberships to this principal (builder). The groups flow
    /// into [`access_context`](Self::access_context) so the document-level ACL
    /// layer can match a group-scoped document.
    #[must_use]
    pub fn with_groups<I, S>(mut self, groups: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.groups = groups.into_iter().map(Into::into).collect();
        self
    }

    /// Whether this principal may act at `min` or above.
    #[must_use]
    pub fn has_role(&self, min: Role) -> bool {
        self.role >= min
    }

    /// Map this principal to the document-level [`AccessContext`] used by the
    /// knowledge-retrieval ACL layer. The user id **and** the principal's groups
    /// carry through, so a retrieval as this principal can match a document
    /// scoped to the user *or* to any group the principal belongs to (the JWT
    /// `groups` claim — see [`Claims`]). The principal's [`org_id`](Self::org_id)
    /// is also carried as the context's `organization_id`, so a multi-tenant
    /// host adapter's `knowledge_for_access` can scope retrieval to this
    /// principal's tenant (the built-in single-tenant ACL ignores it).
    #[must_use]
    pub fn access_context(&self) -> AccessContext {
        AccessContext::new(Some(self.user_id.clone()), self.groups.clone())
            .with_organization_id(self.org_id.clone())
    }
}

/// Why authentication / authorization failed. Maps cleanly to HTTP status in the
/// admin API: [`Unauthenticated`](AuthError::Unauthenticated) /
/// [`InvalidToken`](AuthError::InvalidToken) / [`MissingRole`](AuthError::MissingRole)
/// → 401; [`Forbidden`](AuthError::Forbidden) → 403;
/// [`Misconfigured`](AuthError::Misconfigured) is a server-config error surfaced
/// at startup (never to a client).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    /// No bearer token was presented.
    Unauthenticated,
    /// A token was presented but failed validation (bad signature, expired,
    /// wrong issuer/audience, malformed).
    InvalidToken(String),
    /// The token validated but carried no usable role claim.
    MissingRole(String),
    /// The principal is authenticated but lacks the required role.
    Forbidden {
        /// The role the route requires.
        required: Role,
        /// The role the principal actually has.
        actual: Role,
    },
    /// The verifier is misconfigured (e.g. `AUTH_MODE=jwt` with no key). A
    /// startup/server error, never a client-facing one.
    Misconfigured(String),
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuthError::Unauthenticated => f.write_str("missing bearer token"),
            AuthError::InvalidToken(m) => write!(f, "invalid token: {m}"),
            AuthError::MissingRole(m) => write!(f, "missing or invalid role claim: {m}"),
            AuthError::Forbidden { required, actual } => {
                write!(f, "forbidden: requires {required}, principal is {actual}")
            }
            AuthError::Misconfigured(m) => write!(f, "auth misconfigured: {m}"),
        }
    }
}

impl std::error::Error for AuthError {}

/// The single auth seam: turn a bearer token into a [`Principal`].
///
/// Implemented by [`JwtVerifier`] (BYO), [`SmooIdentityVerifier`] (hosted), and
/// [`NoAuthVerifier`] (dev). `Send + Sync` so a single verifier rides on the
/// shared server state across connections.
pub trait AuthVerifier: Send + Sync {
    /// Validate `bearer_token` (the raw token, **without** the `Bearer ` prefix)
    /// and return the authenticated [`Principal`].
    ///
    /// # Errors
    /// Returns [`AuthError::InvalidToken`] / [`AuthError::MissingRole`] when the
    /// token is present but unusable, or [`AuthError::Unauthenticated`] when it
    /// is empty.
    fn verify(&self, bearer_token: &str) -> Result<Principal, AuthError>;

    /// A short label for logs/metrics (never includes secrets).
    fn mode(&self) -> &'static str;
}

/// Whether an [`AuthVerifier::mode`] denotes a deployment flavor with **no
/// per-user identity concept** — local/dev single-tenant, where every connection
/// is the same human:
///
/// - `none` — [`NoAuthVerifier`], `AUTH_MODE=none` dev,
/// - `disabled` — [`AdminDisabledVerifier`], nothing configured (`/ws` still serves),
/// - `local-token` — [`LocalTokenVerifier`], the single-user daemon / `LocalServer`.
///
/// These are the **only** flavors where conversation reads run unscoped. Every
/// other mode (`jwt` / `jwks` / `smoo` / `trusted`) is multi-user, so reads are
/// scoped to the principal's [`email`](Principal::email) and **fail closed**
/// when there isn't one. An unknown mode is treated as multi-user — a new
/// verifier fails closed rather than silently unscoped.
#[must_use]
pub fn is_single_user_mode(mode: &str) -> bool {
    matches!(mode, "none" | "disabled" | "local-token")
}

/// The JWT claim shape both [`JwtVerifier`] and [`SmooIdentityVerifier`] decode.
/// `org` is the canonical org claim with `org_id` accepted as an alias (SST
/// OpenAuth and Smoo both emit one or the other).
#[derive(Debug, Deserialize)]
struct Claims {
    sub: String,
    #[serde(default)]
    org: Option<String>,
    #[serde(default)]
    org_id: Option<String>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    name: Option<String>,
    /// The principal's email — the per-user conversation scope key. Optional;
    /// absent ⇒ conversation reads fail closed (see [`Principal::email`]).
    #[serde(default)]
    email: Option<String>,
    /// Group memberships (entitlements) — the document-level ACL layer matches
    /// these against a document's group allow-list. Optional; absent ⇒ no group
    /// entitlements (the principal sees only org-public + user-scoped docs).
    #[serde(default)]
    groups: Vec<String>,
}

impl Claims {
    /// Resolve the org id from `org` (preferred) or `org_id` (alias).
    fn org_id(&self) -> Option<String> {
        self.org.clone().or_else(|| self.org_id.clone())
    }

    /// Build a [`Principal`], failing if the role is absent/unknown or no org id
    /// is present.
    fn into_principal(self) -> Result<Principal, AuthError> {
        let role = match &self.role {
            Some(r) => Role::parse(r)?,
            None => return Err(AuthError::MissingRole("no 'role' claim".to_string())),
        };
        let org_id = self
            .org_id()
            .ok_or_else(|| AuthError::InvalidToken("no 'org'/'org_id' claim".to_string()))?;
        Ok(Principal {
            user_id: self.sub,
            org_id,
            role,
            display_name: self.name,
            email: self.email.filter(|e| !e.trim().is_empty()),
            groups: self.groups,
        })
    }
}

/// The signing-key material a [`JwtVerifier`] validates against. Built from env
/// by [`AuthConfig`]; never logged.
enum VerifyKey {
    /// HS256 shared secret.
    Hs256(Box<DecodingKey>),
    /// RS256 public key (PEM). Structural support — the gateway/IdP signs, we
    /// verify with the public half.
    Rs256(Box<DecodingKey>),
}

/// How a [`JwtVerifier`] resolves the verification key for a token.
///
/// - [`Static`](JwtBackend::Static) — a single fixed key (HS256 secret or RS256
///   PEM) with a pre-built [`Validation`]. The original BYO path, unchanged.
/// - [`Jwks`](JwtBackend::Jwks) — keys are pulled (and cached) from the issuer's
///   published JWKS, selected per-token by `kid`. Supports **any** JWS algorithm
///   the JWKS advertises (ES256/ES384/RS256/PS256/EdDSA/…), which is what lets
///   `auth.smoo.ai`'s **ES256** tokens validate. See [`JwksVerifier`].
enum JwtBackend {
    Static {
        key: VerifyKey,
        validation: Validation,
    },
    Jwks(JwksVerifier),
}

/// Validates a JWT and extracts a [`Principal`]. The **BYO** path: SST OpenAuth
/// (or any OIDC IdP) issues the token; this verifies signature + standard claims
/// and maps `sub`→`user_id`, `org`/`org_id`→`org_id`, `role`→[`Role`],
/// `name`→`display_name`.
///
/// Two backends (see [`JwtBackend`]): a **static** key (HS256/RS256) or a
/// **JWKS**-backed multi-algorithm verifier that fetches + caches the issuer's
/// keys and selects one per-token by `kid`.
pub struct JwtVerifier {
    backend: JwtBackend,
}

impl JwtVerifier {
    /// An HS256 verifier over a shared secret. Optionally constrains `iss`/`aud`.
    #[must_use]
    pub fn hs256(secret: &[u8], issuer: Option<String>, audience: Option<String>) -> Self {
        let mut validation = Validation::new(Algorithm::HS256);
        configure_validation(&mut validation, issuer, audience);
        Self {
            backend: JwtBackend::Static {
                key: VerifyKey::Hs256(Box::new(DecodingKey::from_secret(secret))),
                validation,
            },
        }
    }

    /// An RS256 verifier over a PEM-encoded public key. Optionally constrains
    /// `iss`/`aud`. The static BYO path; for issuers that publish a JWKS (and
    /// possibly rotate keys or sign with ES256) use [`JwtVerifier::jwks`].
    ///
    /// # Errors
    /// Returns [`AuthError::Misconfigured`] if the PEM can't be parsed.
    pub fn rs256(
        public_key_pem: &[u8],
        issuer: Option<String>,
        audience: Option<String>,
    ) -> Result<Self, AuthError> {
        let key = DecodingKey::from_rsa_pem(public_key_pem)
            .map_err(|e| AuthError::Misconfigured(format!("invalid RS256 public key: {e}")))?;
        let mut validation = Validation::new(Algorithm::RS256);
        configure_validation(&mut validation, issuer, audience);
        Ok(Self {
            backend: JwtBackend::Static {
                key: VerifyKey::Rs256(Box::new(key)),
                validation,
            },
        })
    }

    /// A JWKS-backed verifier: keys are fetched + cached from `jwks_url` and
    /// selected per-token by `kid`, so **any** advertised algorithm
    /// (ES256/RS256/…) and key rotation work without a redeploy. Optionally
    /// constrains `iss`/`aud`.
    #[must_use]
    pub fn jwks(
        jwks_url: impl Into<String>,
        issuer: Option<String>,
        audience: Option<String>,
    ) -> Self {
        Self {
            backend: JwtBackend::Jwks(JwksVerifier::from_url(jwks_url, issuer, audience)),
        }
    }

    /// A JWKS-backed verifier over a caller-supplied [`JwksFetcher`] (lets tests
    /// inject an in-memory [`JwkSet`] with no network). Optionally constrains
    /// `iss`/`aud`.
    #[must_use]
    pub fn jwks_with_fetcher(
        fetcher: Arc<dyn JwksFetcher>,
        issuer: Option<String>,
        audience: Option<String>,
    ) -> Self {
        Self {
            backend: JwtBackend::Jwks(JwksVerifier::with_fetcher(fetcher, issuer, audience)),
        }
    }

    /// Decode + validate, returning the [`Principal`]. Shared by
    /// [`SmooIdentityVerifier`].
    fn decode_principal(&self, token: &str) -> Result<Principal, AuthError> {
        match &self.backend {
            JwtBackend::Static { key, validation } => {
                if token.trim().is_empty() {
                    return Err(AuthError::Unauthenticated);
                }
                let key = match key {
                    VerifyKey::Hs256(k) | VerifyKey::Rs256(k) => k.as_ref(),
                };
                let data = decode::<Claims>(token, key, validation)
                    .map_err(|e| AuthError::InvalidToken(e.to_string()))?;
                data.claims.into_principal()
            }
            JwtBackend::Jwks(v) => v.decode_principal(token),
        }
    }
}

/// Apply shared validation defaults: require `exp` + `sub`, and constrain
/// `iss`/`aud` only when configured (otherwise `validate_aud` is turned off so a
/// token without an `aud` claim isn't spuriously rejected).
fn configure_validation(
    validation: &mut Validation,
    issuer: Option<String>,
    audience: Option<String>,
) {
    validation.set_required_spec_claims(&["exp", "sub"]);
    match audience {
        Some(aud) => {
            validation.validate_aud = true;
            validation.aud = Some(HashSet::from([aud]));
        }
        // No configured audience ⇒ don't validate it (the default `true` would
        // reject any token lacking an `aud` claim).
        None => validation.validate_aud = false,
    }
    if let Some(iss) = issuer {
        validation.iss = Some(HashSet::from([iss]));
    }
}

// ---- JWKS-backed verification ------------------------------------------------

/// How long a fetched [`JwkSet`] is served from cache before a refresh. Reads on
/// the hot path are local-memory; the network round-trip happens at most once
/// per this interval (plus on an unknown `kid` — see [`JwksKeyStore`]).
const DEFAULT_JWKS_TTL: Duration = Duration::from_secs(300);
/// Floor between JWKS network fetches, so a stream of tokens carrying an unknown
/// `kid` (or a malformed token) can't turn into a fetch-per-request storm.
const DEFAULT_JWKS_MIN_REFRESH: Duration = Duration::from_secs(30);
/// Timeout for the JWKS HTTP fetch — a hung issuer must not stall auth forever.
const JWKS_HTTP_TIMEOUT: Duration = Duration::from_secs(5);

/// Fetches a [`JwkSet`]. The seam that lets [`JwksKeyStore`] pull keys from an
/// HTTP issuer in production ([`HttpJwksFetcher`]) and from an in-memory set in
/// tests ([`StaticJwksFetcher`]) — so the verification logic is exercised with
/// **no network**.
///
/// `fetch` is synchronous so [`AuthVerifier::verify`] can stay synchronous (no
/// per-request `await`): the real HTTP impl runs its blocking call on a
/// dedicated thread, and the result is cached, so the common path is a local
/// read.
pub trait JwksFetcher: Send + Sync {
    /// Fetch the current [`JwkSet`] from the source.
    ///
    /// # Errors
    /// Returns [`AuthError::InvalidToken`] on a network / parse failure (treated
    /// like an unverifiable token) or [`AuthError::Misconfigured`] for a client
    /// build error.
    fn fetch(&self) -> Result<JwkSet, AuthError>;
}

/// An in-memory [`JwksFetcher`] — returns a fixed [`JwkSet`]. Used by tests (and
/// any caller that already holds the keys) to drive the JWKS path offline.
pub struct StaticJwksFetcher {
    set: JwkSet,
}

impl StaticJwksFetcher {
    /// Wrap an already-parsed [`JwkSet`].
    #[must_use]
    pub fn new(set: JwkSet) -> Self {
        Self { set }
    }

    /// Parse a JWKS JSON document (`{"keys":[…]}`) into a fetcher.
    ///
    /// # Errors
    /// Returns [`AuthError::InvalidToken`] if the JSON isn't a valid JWKS.
    pub fn from_json(json: &str) -> Result<Self, AuthError> {
        Ok(Self {
            set: parse_jwks(json)?,
        })
    }
}

impl JwksFetcher for StaticJwksFetcher {
    fn fetch(&self) -> Result<JwkSet, AuthError> {
        Ok(self.set.clone())
    }
}

/// The production [`JwksFetcher`]: an HTTP GET of the issuer's JWKS endpoint.
///
/// The blocking fetch runs on a freshly-spawned OS thread so it is safe to call
/// from **anywhere** — including from inside a Tokio worker (where building a
/// blocking reqwest client would otherwise panic) and from the synchronous
/// [`AuthVerifier::verify`]. It only runs on a cache miss / TTL refresh, so it is
/// off the hot path.
struct HttpJwksFetcher {
    url: String,
    timeout: Duration,
}

impl HttpJwksFetcher {
    fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            timeout: JWKS_HTTP_TIMEOUT,
        }
    }
}

impl JwksFetcher for HttpJwksFetcher {
    fn fetch(&self) -> Result<JwkSet, AuthError> {
        let url = self.url.clone();
        let timeout = self.timeout;
        // A fresh OS thread has no ambient Tokio runtime, so constructing the
        // blocking reqwest client here can never panic, regardless of the
        // caller's context.
        std::thread::spawn(move || -> Result<JwkSet, AuthError> {
            install_ring_crypto_provider();
            let client = reqwest::blocking::Client::builder()
                .timeout(timeout)
                .build()
                .map_err(|e| AuthError::Misconfigured(format!("JWKS HTTP client build: {e}")))?;
            let resp = client
                .get(&url)
                .send()
                .map_err(|e| AuthError::InvalidToken(format!("JWKS fetch ({url}) failed: {e}")))?;
            if !resp.status().is_success() {
                return Err(AuthError::InvalidToken(format!(
                    "JWKS fetch ({url}) returned HTTP {}",
                    resp.status()
                )));
            }
            let body = resp
                .text()
                .map_err(|e| AuthError::InvalidToken(format!("JWKS read ({url}) failed: {e}")))?;
            parse_jwks(&body)
        })
        .join()
        .map_err(|_| AuthError::Misconfigured("JWKS fetch thread panicked".to_string()))?
    }
}

/// Parse a JWKS JSON document into a [`JwkSet`].
fn parse_jwks(body: &str) -> Result<JwkSet, AuthError> {
    serde_json::from_str::<JwkSet>(body)
        .map_err(|e| AuthError::InvalidToken(format!("invalid JWKS JSON: {e}")))
}

/// Install the `ring` rustls [`CryptoProvider`](rustls::crypto::CryptoProvider)
/// as the process default, once. The workspace graph carries both `ring` and
/// `aws-lc-rs`, so rustls 0.23 can't auto-pick a provider; the JWKS HTTPS fetch
/// needs one installed before its first TLS handshake. Idempotent + cheap.
fn install_ring_crypto_provider() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// The cached keyset + when it was last fetched.
struct CachedJwks {
    set: Arc<JwkSet>,
    fetched_at: Option<Instant>,
}

/// A TTL-cached, rotation-aware [`JwkSet`] behind a [`JwksFetcher`].
///
/// - **Cache**: the parsed keyset is held in an [`RwLock`]; the hot path is a
///   read lock + a `kid` lookup — no network, no `await`.
/// - **TTL refresh**: when the cache is older than [`ttl`](Self::ttl) the next
///   lookup refetches.
/// - **Rotation (refresh-on-unknown-`kid`)**: a token whose `kid` isn't in the
///   cache triggers a refetch, so a key the issuer just rotated in is picked up
///   **without a redeploy**. A [`min_refresh`](Self::min_refresh) floor keeps a
///   bad/unknown `kid` from turning into a fetch storm.
struct JwksKeyStore {
    fetcher: Arc<dyn JwksFetcher>,
    cached: RwLock<CachedJwks>,
    ttl: Duration,
    min_refresh: Duration,
}

impl JwksKeyStore {
    fn new(fetcher: Arc<dyn JwksFetcher>, ttl: Duration, min_refresh: Duration) -> Self {
        Self {
            fetcher,
            cached: RwLock::new(CachedJwks {
                set: Arc::new(JwkSet { keys: Vec::new() }),
                fetched_at: None,
            }),
            ttl,
            min_refresh,
        }
    }

    /// Resolve the [`Jwk`] for `kid`, refreshing the cache on a stale TTL or an
    /// unknown `kid` (rotation). With no `kid`, a single-key JWKS resolves to its
    /// one key; an ambiguous (multi-key) JWKS requires a `kid`.
    fn key_for(&self, kid: Option<&str>) -> Result<Jwk, AuthError> {
        // Hot path: a fresh cache that already has the key.
        {
            let r = self.read_cache();
            if r.fetched_at.is_some_and(|t| t.elapsed() < self.ttl) {
                if let Some(jwk) = find_jwk(&r.set, kid) {
                    return Ok(jwk);
                }
            }
        }
        // Stale TTL, never-fetched, or unknown kid → (rate-limited) refresh.
        self.maybe_refresh()?;
        let r = self.read_cache();
        find_jwk(&r.set, kid).ok_or_else(|| match kid {
            Some(k) => AuthError::InvalidToken(format!("no JWK matching kid '{k}' in issuer JWKS")),
            None => AuthError::InvalidToken(
                "token has no 'kid' and the issuer JWKS does not have exactly one key".to_string(),
            ),
        })
    }

    /// Refetch the JWKS unless the last fetch is more recent than `min_refresh`
    /// (the storm guard). A `None` `fetched_at` (never fetched) always fetches.
    fn maybe_refresh(&self) -> Result<(), AuthError> {
        if let Some(t) = self.read_cache().fetched_at {
            if t.elapsed() < self.min_refresh {
                return Ok(());
            }
        }
        let set = self.fetcher.fetch()?;
        let mut w = self
            .cached
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        w.set = Arc::new(set);
        w.fetched_at = Some(Instant::now());
        Ok(())
    }

    fn read_cache(&self) -> std::sync::RwLockReadGuard<'_, CachedJwks> {
        self.cached
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Find the [`Jwk`] for `kid` in a [`JwkSet`] (or the sole key when `kid` is
/// `None`).
fn find_jwk(set: &JwkSet, kid: Option<&str>) -> Option<Jwk> {
    match kid {
        Some(k) => set.find(k).cloned(),
        None if set.keys.len() == 1 => set.keys.first().cloned(),
        None => None,
    }
}

/// Resolve the algorithm to validate with: the **JWK-declared** `alg` when the
/// key carries one (pins verification to the issuer's intended algorithm,
/// closing the JWS algorithm-confusion gap), otherwise the token header's `alg`
/// (still constrained to the selected key's type by `DecodingKey::from_jwk`).
fn resolve_jwk_alg(jwk: &Jwk, header_alg: Algorithm) -> Result<Algorithm, AuthError> {
    match jwk.common.key_algorithm {
        Some(ka) => Algorithm::from_str(&ka.to_string())
            .map_err(|_| AuthError::InvalidToken(format!("unsupported JWK algorithm '{ka}'"))),
        None => Ok(header_alg),
    }
}

/// Validates a JWT against the issuer's **published JWKS** — fetched, cached, and
/// rotation-aware (see [`JwksKeyStore`]). Selects the signing key per-token by
/// `kid`, builds a [`DecodingKey`] from the matching [`Jwk`], and validates with
/// the key's algorithm — so **any** JWS algorithm the issuer advertises works
/// (ES256/ES384/RS256/PS256/EdDSA/…), not just a static RS256 PEM.
///
/// This is what makes `auth.smoo.ai` (the `smoo` issuer, **ES256**) verifiable.
/// `verify` stays synchronous: the keyset is read from cache; the network fetch
/// happens at most once per TTL (plus on a never-seen `kid`).
pub struct JwksVerifier {
    store: JwksKeyStore,
    issuer: Option<String>,
    audience: Option<String>,
}

impl JwksVerifier {
    /// A verifier that pulls keys from `jwks_url` over HTTP (cached, TTL +
    /// rotation refresh). Optionally constrains `iss`/`aud`.
    #[must_use]
    pub fn from_url(
        jwks_url: impl Into<String>,
        issuer: Option<String>,
        audience: Option<String>,
    ) -> Self {
        Self::with_fetcher(Arc::new(HttpJwksFetcher::new(jwks_url)), issuer, audience)
    }

    /// A verifier over a caller-supplied [`JwksFetcher`] (tests inject an
    /// in-memory [`JwkSet`]). Optionally constrains `iss`/`aud`.
    #[must_use]
    pub fn with_fetcher(
        fetcher: Arc<dyn JwksFetcher>,
        issuer: Option<String>,
        audience: Option<String>,
    ) -> Self {
        Self::with_policy(
            fetcher,
            issuer,
            audience,
            DEFAULT_JWKS_TTL,
            DEFAULT_JWKS_MIN_REFRESH,
        )
    }

    /// Full constructor exposing the cache `ttl` + `min_refresh` floor (tests
    /// drive rotation timing through this).
    #[must_use]
    pub fn with_policy(
        fetcher: Arc<dyn JwksFetcher>,
        issuer: Option<String>,
        audience: Option<String>,
        ttl: Duration,
        min_refresh: Duration,
    ) -> Self {
        Self {
            store: JwksKeyStore::new(fetcher, ttl, min_refresh),
            issuer,
            audience,
        }
    }

    /// Decode + validate `token` against the cached JWKS, returning the
    /// [`Principal`].
    fn decode_principal(&self, token: &str) -> Result<Principal, AuthError> {
        if token.trim().is_empty() {
            return Err(AuthError::Unauthenticated);
        }
        let header = decode_header(token)
            .map_err(|e| AuthError::InvalidToken(format!("bad JWT header: {e}")))?;
        let jwk = self.store.key_for(header.kid.as_deref())?;
        let alg = resolve_jwk_alg(&jwk, header.alg)?;
        let key = DecodingKey::from_jwk(&jwk)
            .map_err(|e| AuthError::InvalidToken(format!("unusable JWK: {e}")))?;
        let mut validation = Validation::new(alg);
        configure_validation(&mut validation, self.issuer.clone(), self.audience.clone());
        let data = decode::<Claims>(token, &key, &validation)
            .map_err(|e| AuthError::InvalidToken(e.to_string()))?;
        data.claims.into_principal()
    }
}

impl AuthVerifier for JwksVerifier {
    fn verify(&self, bearer_token: &str) -> Result<Principal, AuthError> {
        self.decode_principal(bearer_token)
    }

    fn mode(&self) -> &'static str {
        "jwks"
    }
}

impl AuthVerifier for JwtVerifier {
    fn verify(&self, bearer_token: &str) -> Result<Principal, AuthError> {
        self.decode_principal(bearer_token)
    }

    fn mode(&self) -> &'static str {
        "jwt"
    }
}

/// Validates a **Smoo-issued** token — the hosted path (lom.smoo.ai wires Smoo's
/// identity). Implemented as JWT validation keyed to Smoo's issuer/audience,
/// reusing [`JwtVerifier`]'s internals.
///
/// ## Live introspection (hosted, stubbed)
///
/// The fully-hosted variant would call Smoo's auth server `/introspect` endpoint
/// (RFC 7662) to validate an opaque token and pull the principal. That requires
/// a network round-trip + a client credential, so it is intentionally **not**
/// implemented here: [`SmooIdentityVerifier::introspect`] documents the contract
/// and returns [`AuthError::Misconfigured`] until the introspection client is
/// wired. The JWT form below is the one exercised in tests + the default hosted
/// deployment (Smoo signs a JWT; we verify it locally with Smoo's public key /
/// shared secret — no per-request network call).
pub struct SmooIdentityVerifier {
    inner: JwtVerifier,
}

impl SmooIdentityVerifier {
    /// A Smoo-identity verifier over an HS256 shared secret, keyed to Smoo's
    /// issuer + audience.
    #[must_use]
    pub fn hs256(secret: &[u8], issuer: String, audience: Option<String>) -> Self {
        Self {
            inner: JwtVerifier::hs256(secret, Some(issuer), audience),
        }
    }

    /// A Smoo-identity verifier over an RS256 public key, keyed to Smoo's
    /// issuer + audience.
    ///
    /// # Errors
    /// Returns [`AuthError::Misconfigured`] if the PEM can't be parsed.
    pub fn rs256(
        public_key_pem: &[u8],
        issuer: String,
        audience: Option<String>,
    ) -> Result<Self, AuthError> {
        Ok(Self {
            inner: JwtVerifier::rs256(public_key_pem, Some(issuer), audience)?,
        })
    }

    /// A Smoo-identity verifier backed by Smoo's **published JWKS** — the path
    /// that makes real `auth.smoo.ai` tokens (signed **ES256**, `kty: EC`)
    /// verifiable. Keys are fetched + cached from `jwks_url` (typically
    /// `{issuer}/.well-known/jwks.json`) and selected per-token by `kid`, so key
    /// rotation needs no redeploy and any advertised algorithm works. Keyed to
    /// Smoo's issuer + audience.
    #[must_use]
    pub fn jwks(jwks_url: impl Into<String>, issuer: String, audience: Option<String>) -> Self {
        Self {
            inner: JwtVerifier::jwks(jwks_url, Some(issuer), audience),
        }
    }

    /// A Smoo-identity verifier over a caller-supplied [`JwksFetcher`] (tests
    /// inject an in-memory [`JwkSet`]; no network). Keyed to Smoo's issuer +
    /// audience.
    #[must_use]
    pub fn jwks_with_fetcher(
        fetcher: Arc<dyn JwksFetcher>,
        issuer: String,
        audience: Option<String>,
    ) -> Self {
        Self {
            inner: JwtVerifier::jwks_with_fetcher(fetcher, Some(issuer), audience),
        }
    }

    /// Live token introspection (RFC 7662) against Smoo's auth server.
    ///
    /// **Not implemented**: this is the opaque-token hosted variant, which needs
    /// a network call to `{auth_server}/introspect` with a client credential and
    /// a parse of the introspection response into a [`Principal`]. Wiring it is
    /// the follow-up; until then this returns [`AuthError::Misconfigured`] so a
    /// caller can never mistake the stub for a working validator.
    ///
    /// # Errors
    /// Always returns [`AuthError::Misconfigured`] (stub).
    pub fn introspect(&self, _opaque_token: &str) -> Result<Principal, AuthError> {
        Err(AuthError::Misconfigured(
            "live token introspection is not wired; use the JWT form (Smoo signs a JWT we verify \
             locally) or implement the /introspect client"
                .to_string(),
        ))
    }
}

impl AuthVerifier for SmooIdentityVerifier {
    fn verify(&self, bearer_token: &str) -> Result<Principal, AuthError> {
        self.inner.decode_principal(bearer_token)
    }

    fn mode(&self) -> &'static str {
        "smoo"
    }
}

/// **Dev-only** verifier: returns a fixed `Admin` principal for *any* token
/// (including none). Reachable only via an explicit `AUTH_MODE=none`
/// ([`AuthConfig::from_env`]) so it can never be the silent production default.
pub struct NoAuthVerifier {
    principal: Principal,
}

impl NoAuthVerifier {
    /// A no-auth verifier returning an `Admin` principal in `org_id`.
    #[must_use]
    pub fn new(org_id: impl Into<String>) -> Self {
        Self {
            principal: Principal::new(
                "dev-admin",
                org_id,
                Role::Admin,
                Some("Dev Admin (AUTH_MODE=none)".to_string()),
            ),
        }
    }
}

impl Default for NoAuthVerifier {
    fn default() -> Self {
        Self::new("dev-org")
    }
}

impl AuthVerifier for NoAuthVerifier {
    fn verify(&self, _bearer_token: &str) -> Result<Principal, AuthError> {
        Ok(self.principal.clone())
    }

    fn mode(&self) -> &'static str {
        "none"
    }
}

/// **Local single-user** verifier — the auth for the *local deployment flavor*.
///
/// Holds one shared secret (the local daemon auto-provisions it). The presented
/// token must equal the secret, compared in **constant time**; on match the
/// connection runs as a fixed local `Admin` principal, and on mismatch/empty it
/// **fails closed**. This gates stray local processes from connecting to the
/// loopback/tailnet server without dragging in the multi-tenant JWT/IdP
/// machinery — exactly the posture a single-user always-on daemon wants.
///
/// The token rides in the **same slot** a JWT would: the `/ws` `?token=` query
/// param (reference server) or the `send_message` `token` field (Lambda), so all
/// existing transport plumbing is reused.
pub struct LocalTokenVerifier {
    secret: String,
    principal: Principal,
}

impl LocalTokenVerifier {
    /// A verifier over `secret`; matched connections run as a local `Admin`.
    #[must_use]
    pub fn new(secret: impl Into<String>) -> Self {
        Self {
            secret: secret.into(),
            principal: Principal::new(
                "local",
                "local",
                Role::Admin,
                Some("Local user".to_string()),
            ),
        }
    }
}

impl AuthVerifier for LocalTokenVerifier {
    fn verify(&self, bearer_token: &str) -> Result<Principal, AuthError> {
        if bearer_token.is_empty() {
            return Err(AuthError::Unauthenticated);
        }
        if local_token_eq(bearer_token.as_bytes(), self.secret.as_bytes()) {
            Ok(self.principal.clone())
        } else {
            Err(AuthError::InvalidToken("local token mismatch".to_string()))
        }
    }

    fn mode(&self) -> &'static str {
        "local-token"
    }
}

/// Length-aware constant-time byte comparison, so the local-token check leaks
/// neither length nor content through timing.
fn local_token_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

/// **Tokenless trusted-upstream** verifier — `AUTH_MODE=trusted`.
///
/// For the **proxied-integration** deployment shape: an existing application's
/// backend has *already* authenticated the user and proxies smooth-operator over
/// a trusted/internal network. That upstream forwards the user's identity
/// (`sub` / `org` / `role` / `groups`); smooth-operator **trusts** it **without
/// any signature verification** — the upstream owns identity *and* token
/// lifetime, so there is no signature to check and no `exp` to enforce.
///
/// ## Wire format — identity in the same slot a token would ride
///
/// The forwarded identity rides in the **exact same slot** a JWT would: the
/// `/ws` `?token=` query param (reference server) or the `send_message` `token`
/// field (Lambda). So *all* the existing transport plumbing is reused — the only
/// difference from [`JwtVerifier`] is **trust, don't verify**.
///
/// The value is **`base64url(JSON)`** of the [`Claims`] shape, e.g.
/// `base64url({"sub":"u1","org":"acme","role":"basic","groups":["github:acme/secret"]})`.
/// base64url is used (not raw JSON) so the blob survives the query-string and
/// JSON-string transports cleanly without escaping. No padding is required
/// (`URL_SAFE_NO_PAD` is accepted; padded `URL_SAFE` is also tolerated).
///
/// ## Security boundary — this is **trust without verification**
///
/// `AUTH_MODE=trusted` is **only safe when smooth-operator is not directly
/// reachable by clients** — it must be fronted by your authenticated
/// backend/proxy on a trusted network. A client that *can* reach `/ws` directly
/// could forge any identity (any org, any groups). [`AuthConfig::from_env`]
/// emits a loud startup `tracing::warn!` to that effect whenever this mode is
/// selected.
///
/// ## Fail closed — never silently no-auth-admin
///
/// Absent / empty / malformed trusted identity yields an [`AuthError`], which the
/// connect path ([`crate::access_control::AccessContext::anonymous`]) maps to an
/// **anonymous** connection (org-public only) — exactly like the no-token path.
/// Trusted mode **never** degrades to an admin / all-access principal on bad
/// input.
pub struct TrustedIdentityVerifier;

impl TrustedIdentityVerifier {
    /// Construct the trusted-identity verifier (stateless).
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Decode `base64url(JSON)` identity → [`Claims`] → [`Principal`], **without**
    /// any signature or `exp` check.
    fn decode_trusted(forwarded: &str) -> Result<Principal, AuthError> {
        use base64::Engine as _;

        let forwarded = forwarded.trim();
        if forwarded.is_empty() {
            return Err(AuthError::Unauthenticated);
        }
        // Accept unpadded URL-safe (the canonical encoding) and fall back to the
        // padded variant so a caller that pads isn't spuriously rejected.
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(forwarded)
            .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(forwarded))
            .map_err(|e| {
                AuthError::InvalidToken(format!("trusted identity is not valid base64url: {e}"))
            })?;
        let claims: Claims = serde_json::from_slice(&bytes).map_err(|e| {
            AuthError::InvalidToken(format!("trusted identity is not valid claims JSON: {e}"))
        })?;
        // Reuse the exact same Claims→Principal mapping as the JWT path: missing
        // `role` / `org` are still hard errors (which fail closed to anonymous),
        // so a blob that omits them can never become an admin.
        claims.into_principal()
    }
}

impl Default for TrustedIdentityVerifier {
    fn default() -> Self {
        Self::new()
    }
}

impl AuthVerifier for TrustedIdentityVerifier {
    fn verify(&self, forwarded_identity: &str) -> Result<Principal, AuthError> {
        Self::decode_trusted(forwarded_identity)
    }

    fn mode(&self) -> &'static str {
        "trusted"
    }
}

/// Builds the configured [`AuthVerifier`] from the environment — secure by
/// default.
///
/// ## Environment
///
/// | var | default | meaning |
/// | --- | --- | --- |
/// | `AUTH_MODE` | `jwt` | `jwt` (BYO) \| `smoo` (hosted) \| `trusted` (proxied, tokenless — see below) \| `none` (dev only). |
/// | `AUTH_JWT_HS256_SECRET` | — | HS256 shared secret. |
/// | `AUTH_JWT_RS256_PUBLIC_KEY` | — | Static RS256 PEM public key. |
/// | `AUTH_JWT_JWKS_URL` | — | JWKS endpoint to fetch signing keys from (any algorithm — ES256/RS256/…). |
/// | `AUTH_JWT_ISSUER` | — | Required `iss` (optional). Also the JWKS auto-derivation root (`{issuer}/.well-known/jwks.json`). |
/// | `AUTH_JWT_AUDIENCE` | — | Required `aud` (optional). |
/// | `AUTH_DEV_ORG_ID` | `dev-org` | Org id for the `none`-mode admin principal. |
///
/// ## Key-source precedence (`jwt` and `smoo`)
///
/// 1. **Static `AUTH_JWT_RS256_PUBLIC_KEY`** (RS256 PEM) — the BYO path, unchanged.
/// 2. **Static `AUTH_JWT_HS256_SECRET`** (HS256 shared secret).
/// 3. **JWKS** — `AUTH_JWT_JWKS_URL` if set, else derived from the issuer as
///    `{AUTH_JWT_ISSUER}/.well-known/jwks.json`. This is the **ES256-capable**
///    path: keys are fetched + cached and selected per-token by `kid`, so
///    `auth.smoo.ai`'s ES256 tokens verify and key rotation needs no redeploy.
///
/// So `AUTH_MODE=smoo` now needs only `AUTH_JWT_ISSUER` (+ optionally
/// `AUTH_JWT_AUDIENCE`) — no static public key required.
///
/// **Explicitly** setting `AUTH_MODE=jwt`/`smoo` with **no** usable key source
/// (no static key, no JWKS URL, and — for `jwt` — no issuer to derive one) is a
/// hard [`AuthError::Misconfigured`] error — not a silent fall-through to no-auth.
/// Leaving `AUTH_MODE` **unset** with no key source boots the server with the
/// admin API **disabled** ([`AdminDisabledVerifier`]) so `/ws` serves without
/// forcing auth config; `/admin` then returns 401 until configured (or
/// `AUTH_MODE=none` for dev).
///
/// A verifier that rejects every request. The default when neither `AUTH_MODE`
/// nor a key is configured: the server still boots (so `/ws` serves) but the
/// `/admin` API is disabled until an operator sets `AUTH_MODE` + a key, or
/// `AUTH_MODE=none` for local dev. Secure-by-default without hard-failing the
/// whole service over admin config.
#[derive(Debug, Clone, Copy, Default)]
pub struct AdminDisabledVerifier;

impl AuthVerifier for AdminDisabledVerifier {
    fn verify(&self, _bearer_token: &str) -> Result<Principal, AuthError> {
        Err(AuthError::InvalidToken(
            "admin API disabled: set AUTH_MODE=jwt|smoo + a key, or AUTH_MODE=none for dev"
                .to_string(),
        ))
    }

    fn mode(&self) -> &'static str {
        "disabled"
    }
}

pub struct AuthConfig;

impl AuthConfig {
    /// Build the verifier the env selects. Reads keys from env (never logs them).
    ///
    /// # Errors
    /// Returns [`AuthError::Misconfigured`] for an unknown `AUTH_MODE`, or for
    /// `jwt`/`smoo` without a usable key.
    pub fn from_env() -> Result<Box<dyn AuthVerifier>, AuthError> {
        let raw_mode = std::env::var("AUTH_MODE")
            .ok()
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty());
        let mode_explicit = raw_mode.is_some();
        let mode = raw_mode.unwrap_or_else(|| "jwt".to_string());

        let issuer = env_nonempty("AUTH_JWT_ISSUER");
        let audience = env_nonempty("AUTH_JWT_AUDIENCE");

        match mode.as_str() {
            "none" => {
                let org = env_nonempty("AUTH_DEV_ORG_ID").unwrap_or_else(|| "dev-org".to_string());
                Ok(Box::new(NoAuthVerifier::new(org)))
            }
            "trusted" => {
                // Reached ONLY by an explicit `AUTH_MODE=trusted`. Identity is
                // taken from the upstream caller WITHOUT verification, so warn
                // loudly at startup that this is only safe behind a trusted proxy.
                tracing::warn!(
                    "AUTH_MODE=trusted — identity is trusted from the upstream caller WITHOUT \
                     verification; ONLY safe when smooth-operator is not directly reachable by \
                     clients (front it with your authenticated backend/proxy). Bad/absent \
                     identity fails closed to anonymous (org-public only), never admin."
                );
                Ok(Box::new(TrustedIdentityVerifier::new()))
            }
            "jwt" => match Self::build_jwt(issuer, audience) {
                Ok(v) => Ok(Box::new(v)),
                // Default mode (AUTH_MODE unset) with no key: boot with the admin
                // API disabled rather than hard-failing the whole server.
                Err(AuthError::Misconfigured(_)) if !mode_explicit => {
                    tracing::warn!(
                        "admin API disabled: no AUTH_MODE/key configured — /ws serves, /admin returns 401. Set AUTH_MODE=jwt + a key (or AUTH_MODE=none for dev) to enable it."
                    );
                    Ok(Box::new(AdminDisabledVerifier))
                }
                // Explicitly choosing AUTH_MODE=jwt with no key stays a loud startup error.
                Err(e) => Err(e),
            },
            "smoo" => {
                let iss = issuer.ok_or_else(|| {
                    AuthError::Misconfigured(
                        "AUTH_MODE=smoo requires AUTH_JWT_ISSUER (Smoo's issuer)".to_string(),
                    )
                })?;
                if let Some(pem) = env_nonempty("AUTH_JWT_RS256_PUBLIC_KEY") {
                    Ok(Box::new(SmooIdentityVerifier::rs256(
                        pem.as_bytes(),
                        iss,
                        audience,
                    )?))
                } else if let Some(secret) = env_nonempty("AUTH_JWT_HS256_SECRET") {
                    Ok(Box::new(SmooIdentityVerifier::hs256(
                        secret.as_bytes(),
                        iss,
                        audience,
                    )))
                } else {
                    // No static key: verify against Smoo's published JWKS. Smoo
                    // (`auth.smoo.ai`) signs with ES256 (`kty: EC`), which a
                    // static RS256 PEM can't validate — JWKS is the working path.
                    // The issuer is always present here (required above), so the
                    // JWKS URL is always derivable.
                    let url = jwks_source(Some(&iss)).expect("issuer is present for smoo mode");
                    Ok(Box::new(SmooIdentityVerifier::jwks(url, iss, audience)))
                }
            }
            other => Err(AuthError::Misconfigured(format!(
                "unknown AUTH_MODE '{other}' (expected jwt | smoo | trusted | none)"
            ))),
        }
    }

    /// Build a [`JwtVerifier`] from env. Key-source precedence: static RS256 PEM
    /// → static HS256 secret → JWKS (`AUTH_JWT_JWKS_URL`, else
    /// `{AUTH_JWT_ISSUER}/.well-known/jwks.json`).
    fn build_jwt(
        issuer: Option<String>,
        audience: Option<String>,
    ) -> Result<JwtVerifier, AuthError> {
        if let Some(pem) = env_nonempty("AUTH_JWT_RS256_PUBLIC_KEY") {
            JwtVerifier::rs256(pem.as_bytes(), issuer, audience)
        } else if let Some(secret) = env_nonempty("AUTH_JWT_HS256_SECRET") {
            Ok(JwtVerifier::hs256(secret.as_bytes(), issuer, audience))
        } else if let Some(url) = jwks_source(issuer.as_deref()) {
            // Any OIDC issuer that publishes a JWKS works (ES256/RS256/…), with
            // no static key in env.
            Ok(JwtVerifier::jwks(url, issuer, audience))
        } else {
            Err(AuthError::Misconfigured(
                "AUTH_MODE=jwt requires AUTH_JWT_RS256_PUBLIC_KEY, AUTH_JWT_HS256_SECRET, \
                 AUTH_JWT_JWKS_URL, or AUTH_JWT_ISSUER (to derive the JWKS URL) \
                 (refusing to fall back to no-auth)"
                    .to_string(),
            ))
        }
    }
}

/// Resolve the JWKS endpoint: an explicit `AUTH_JWT_JWKS_URL` wins, otherwise
/// derive `{issuer}/.well-known/jwks.json` from the configured issuer (the
/// standard OIDC location `auth.smoo.ai` serves). `None` when neither is set.
fn jwks_source(issuer: Option<&str>) -> Option<String> {
    if let Some(url) = env_nonempty("AUTH_JWT_JWKS_URL") {
        return Some(url);
    }
    issuer.map(|iss| format!("{}/.well-known/jwks.json", iss.trim_end_matches('/')))
}

/// Read an env var, returning `None` when absent or empty/whitespace.
fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{encode, EncodingKey, Header};
    use serde_json::json;

    const SECRET: &[u8] = b"test-shared-secret-not-a-real-key";

    /// Sign an HS256 token with the given claims object.
    fn sign(claims: serde_json::Value) -> String {
        encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(SECRET),
        )
        .expect("sign")
    }

    /// A far-future expiry so tokens are valid.
    fn future_exp() -> i64 {
        (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp()
    }

    // ---- JWKS / ES256 fixtures -------------------------------------------
    //
    // A locally-generated EC P-256 (ES256) keypair + the matching public JWK,
    // and an RSA-2048 keypair for the static-RS256 regression. All offline: the
    // JWKS path is driven through an injected `JwksFetcher`, never the network.

    /// PKCS#8 EC P-256 private key (test-only; generated with openssl).
    const EC_PRIV_PEM: &str = "-----BEGIN PRIVATE KEY-----\n\
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgS73a4tqPSek9+32c\n\
x0FaP0T8bhMiC5yIvyBGW9qk68ehRANCAAQ7175zcp6KZfPVpFG4a8RI0dtVKNtr\n\
YIF2/Pl3nm1Pb1imLIy4WnLa+vr0nqcC0612yaRg4KWjYj6XdDO9gP+Y\n\
-----END PRIVATE KEY-----\n";
    /// `kid` advertised for the EC public key in the test JWKS.
    const EC_KID: &str = "test-ec-1";
    /// base64url EC public point coords matching `EC_PRIV_PEM`.
    const EC_X: &str = "O9e-c3KeimXz1aRRuGvESNHbVSjba2CBdvz5d55tT28";
    const EC_Y: &str = "WKYsjLhactr6-vSepwLTrXbJpGDgpaNiPpd0M72A_5g";

    /// RSA-2048 keypair for the static-RS256 regression (test-only).
    const RSA_PUB_PEM: &str = "-----BEGIN PUBLIC KEY-----\n\
MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAw0MeIERxU2bLpDNQaSis\n\
nz93wtxbYL3aTVEiHSGCyDysrpIAFQxD8IjXn0lLnf/OlR0IWjBH/6ARsXucXemG\n\
jzZBCpHbna0PAnNXUOOPM88gev/XN9p+MxWPDHnyd1ZtyxAHc5xo0a596Gq3HE9C\n\
QL53nMIYEOBOP5VeUQS68G7DGo+dTQgXrFb98fsqYS3xqeLoYWI+tHYEkzY4DFxb\n\
jdvBvBN65N84pYnk7Pd/vbITvVaDC7pev1E5wvh4Iu/zZy0LBnQPgcMEumcc5cZQ\n\
6Filt8q83ReOIWpmQfNryxgdz7okUvOZSzkYLJscwjkdyBDOcaKxT5O323dd1xm8\n\
6QIDAQAB\n\
-----END PUBLIC KEY-----\n";
    const RSA_PRIV_PEM: &str = "-----BEGIN PRIVATE KEY-----\n\
MIIEvAIBADANBgkqhkiG9w0BAQEFAASCBKYwggSiAgEAAoIBAQDDQx4gRHFTZsuk\n\
M1BpKKyfP3fC3FtgvdpNUSIdIYLIPKyukgAVDEPwiNefSUud/86VHQhaMEf/oBGx\n\
e5xd6YaPNkEKkdudrQ8Cc1dQ448zzyB6/9c32n4zFY8MefJ3Vm3LEAdznGjRrn3o\n\
arccT0JAvnecwhgQ4E4/lV5RBLrwbsMaj51NCBesVv3x+yphLfGp4uhhYj60dgST\n\
NjgMXFuN28G8E3rk3zilieTs93+9shO9VoMLul6/UTnC+Hgi7/NnLQsGdA+BwwS6\n\
ZxzlxlDoWKW3yrzdF44hamZB82vLGB3PuiRS85lLORgsmxzCOR3IEM5xorFPk7fb\n\
d13XGbzpAgMBAAECggEACKe7+SAvicvfsPqZUN/9rt1oWJnd7w7bU1wKUBJBMtEF\n\
soNEP6qYhFv8etIL6QgCxzdPPHgxaNJWlnBtQPht/4EfJvHKM1YNeUVVlH9RxLEk\n\
tm8Kwi4MNAV7nsj1B3csTLj8K5K+TrUWXawFS9rzi90lfixYVr8qmMTtNlgoVSnv\n\
vNsIbEIoqNu4SwIAAmuXTsVoaUcgo8L+UDtTn3LXl4X5Daz6Z54whloMr+YjdoxL\n\
exLSN9Z4sirhoDpUMl9ckmu57stObY2IHsJeMNzmhg8u535GrlyPs+JHYs6lIzWX\n\
O4UT8VOwnkOcudCTL3l8sITJmArzkjSMqSzsiPb65QKBgQD+pLZHfYwfR72aQnLE\n\
Ypwo1SNZBWy2SDeszSgnzTr9u8kPChIgUTmRam7f6++hPe49S0n/BwTm3SXxKZQ+\n\
yySyW9ikmR4qzNhMywL8ViKNcGtuKSrad+KA3Ur4Oq3RzmVDYPMoJ0yiaQW19Yfy\n\
R+L5Y0x9drUWH4vqYqk4FJKg2wKBgQDETWuYq74omGHyNMAXWdAcsW+HA+A21HA2\n\
4jK8X1e8Qdo/ddBZjgr7satzhBYdAa5VOS6unL//Al8eYNHmnvLqLFmReUye7Mp+\n\
c+LxIUzta0M6q4Nnq69ctvMq9WFG/Lj7pUxzuBDk6Q3X/8tu25DoBzmv/iQDP4eY\n\
F9FB4ZcSiwKBgH2GUFx5ZQNeZ/aM3uoz+eqe9mfBps9MVjWWhD7qijPdx8TkH/9S\n\
SuCF6NX1BhEj6DbK0FUo7p+nUDbLWkqB9Tr+z5KD8D0E8XMZeAVPqIS0cCDDpl4/\n\
TqZbb8NhmaGc7ooCVprqlHpS7v+9YyBpk1eAPYpzY9zd/Ci0Ldp5ObaVAoGAOVFh\n\
2XJMVA4qi05byHWxDq/AoOvAzEG7gksKBXbRZ2bTEzSTYZLYIiX+qfwneNDE1p2b\n\
w+CBLzTCEVyz7WL8CuRoQtHoTX9WoRW1bjMLA0gOmVL7S4oV6jyBREnh3Zhtaw0Z\n\
BbD5Pd3O7QMDo5r49McnUPwkB87FCOPrdhEoy4ECgYBCBhrsUic64os42vqIdNc9\n\
y7LwxQbJgj1EELIx1ErXtbWkhqSCYJ4dOOuRn2koc0SXk0Q0fnbQck+8bc4R6FXp\n\
dbzmuAQrASyqJ4cWmKhJyKgZzMfelJVVTnM/5H+mFMSZweNWNN5jn1VbWJNgrZpj\n\
fabZgkSUBnZ7xCln6zeeWQ==\n\
-----END PRIVATE KEY-----\n";

    /// A JWKS JSON document carrying the EC public key (`kid = test-ec-1`).
    fn ec_jwks_json() -> String {
        format!(
            r#"{{"keys":[{{"kty":"EC","crv":"P-256","x":"{EC_X}","y":"{EC_Y}","alg":"ES256","use":"sig","kid":"{EC_KID}"}}]}}"#
        )
    }

    /// Sign an ES256 token with `EC_PRIV_PEM`, stamping the given `kid` header.
    fn sign_es256(claims: serde_json::Value, kid: &str) -> String {
        let mut header = Header::new(Algorithm::ES256);
        header.kid = Some(kid.to_string());
        let key = EncodingKey::from_ec_pem(EC_PRIV_PEM.as_bytes()).expect("ec encoding key");
        encode(&header, &claims, &key).expect("sign es256")
    }

    /// Sign an RS256 token with `RSA_PRIV_PEM`.
    fn sign_rs256(claims: serde_json::Value) -> String {
        let key = EncodingKey::from_rsa_pem(RSA_PRIV_PEM.as_bytes()).expect("rsa encoding key");
        encode(&Header::new(Algorithm::RS256), &claims, &key).expect("sign rs256")
    }

    // (a) An ES256 token verifies against a JWKS holding its EC public key.
    #[test]
    fn jwks_verifier_validates_es256_token() {
        let fetcher = Arc::new(StaticJwksFetcher::from_json(&ec_jwks_json()).expect("jwks"));
        let v = JwksVerifier::with_fetcher(
            fetcher,
            Some("https://auth.smoo.ai".to_string()),
            Some("smoo-api".to_string()),
        );
        let token = sign_es256(
            json!({
                "sub": "user-es",
                "org": "org-es",
                "role": "admin",
                "name": "EC User",
                "iss": "https://auth.smoo.ai",
                "aud": "smoo-api",
                "exp": future_exp(),
            }),
            EC_KID,
        );
        let p = v.verify(&token).expect("verify es256");
        assert_eq!(p.user_id, "user-es");
        assert_eq!(p.org_id, "org-es");
        assert_eq!(p.role, Role::Admin);
        assert_eq!(p.display_name.as_deref(), Some("EC User"));
        assert_eq!(v.mode(), "jwks");
    }

    // (a') The SmooIdentityVerifier (AUTH_MODE=smoo) validates real-shaped ES256
    // tokens through the JWKS path — the actual auth.smoo.ai scenario.
    #[test]
    fn smoo_identity_verifier_validates_es256_via_jwks() {
        let fetcher = Arc::new(StaticJwksFetcher::from_json(&ec_jwks_json()).expect("jwks"));
        let v = SmooIdentityVerifier::jwks_with_fetcher(
            fetcher,
            "https://auth.smoo.ai".to_string(),
            Some("smoo-api".to_string()),
        );
        let token = sign_es256(
            json!({
                "sub": "smoo-user",
                "org": "smoo-org",
                "role": "curator",
                "iss": "https://auth.smoo.ai",
                "aud": "smoo-api",
                "exp": future_exp(),
            }),
            EC_KID,
        );
        let p = v.verify(&token).expect("smoo verify es256");
        assert_eq!(p.user_id, "smoo-user");
        assert_eq!(p.role, Role::Curator);
        assert_eq!(v.mode(), "smoo");
    }

    // (b) The existing static-RS256 path still verifies — behavior-preserving.
    #[test]
    fn static_rs256_path_still_verifies() {
        let v = JwtVerifier::rs256(RSA_PUB_PEM.as_bytes(), None, None).expect("rs256 verifier");
        let token = sign_rs256(json!({
            "sub": "rsa-user",
            "org": "rsa-org",
            "role": "basic",
            "exp": future_exp(),
        }));
        let p = v.verify(&token).expect("verify rs256");
        assert_eq!(p.user_id, "rsa-user");
        assert_eq!(p.role, Role::Basic);
        assert_eq!(v.mode(), "jwt");
    }

    // (c) An unknown `kid` triggers a JWKS refresh — a key the issuer rotates in
    // is picked up without a redeploy (and an absent key fails cleanly).
    #[test]
    fn unknown_kid_triggers_jwks_refresh() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountingFetcher {
            set: Mutex<JwkSet>,
            calls: AtomicUsize,
        }
        impl JwksFetcher for CountingFetcher {
            fn fetch(&self) -> Result<JwkSet, AuthError> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(self.set.lock().unwrap().clone())
            }
        }

        // Start with an empty JWKS (the key has not been published yet).
        let fetcher = Arc::new(CountingFetcher {
            set: Mutex::new(JwkSet { keys: Vec::new() }),
            calls: AtomicUsize::new(0),
        });
        // min_refresh = 0 so the unknown-kid path refreshes immediately in-test.
        let v = JwksVerifier::with_policy(
            fetcher.clone(),
            Some("iss-rot".to_string()),
            None,
            Duration::from_secs(3600),
            Duration::ZERO,
        );
        let token = sign_es256(
            json!({
                "sub": "rot-user",
                "org": "rot-org",
                "role": "basic",
                "iss": "iss-rot",
                "exp": future_exp(),
            }),
            EC_KID,
        );

        // First attempt: the key isn't in the JWKS yet → fails cleanly, but a
        // fetch was attempted.
        assert!(v.verify(&token).is_err());
        let after_first = fetcher.calls.load(Ordering::SeqCst);
        assert!(after_first >= 1, "an initial fetch must have happened");

        // The issuer rotates the EC key in.
        *fetcher.set.lock().unwrap() = parse_jwks(&ec_jwks_json()).expect("jwks");

        // Next verify: the unknown-kid cache miss forces a refresh → the rotated
        // key is found → the token verifies.
        let p = v.verify(&token).expect("verify after rotation");
        assert_eq!(p.user_id, "rot-user");
        assert!(
            fetcher.calls.load(Ordering::SeqCst) > after_first,
            "rotation must have triggered a refetch"
        );
    }

    // (d) Wrong issuer / audience are rejected even with a valid signature.
    #[test]
    fn jwks_rejects_wrong_issuer() {
        let fetcher = Arc::new(StaticJwksFetcher::from_json(&ec_jwks_json()).expect("jwks"));
        let v = JwksVerifier::with_fetcher(
            fetcher,
            Some("https://auth.smoo.ai".to_string()),
            Some("smoo-api".to_string()),
        );
        let token = sign_es256(
            json!({
                "sub": "u", "org": "o", "role": "basic",
                "iss": "https://evil.example", "aud": "smoo-api", "exp": future_exp(),
            }),
            EC_KID,
        );
        assert!(matches!(v.verify(&token), Err(AuthError::InvalidToken(_))));
    }

    #[test]
    fn jwks_rejects_wrong_audience() {
        let fetcher = Arc::new(StaticJwksFetcher::from_json(&ec_jwks_json()).expect("jwks"));
        let v = JwksVerifier::with_fetcher(
            fetcher,
            Some("https://auth.smoo.ai".to_string()),
            Some("smoo-api".to_string()),
        );
        let token = sign_es256(
            json!({
                "sub": "u", "org": "o", "role": "basic",
                "iss": "https://auth.smoo.ai", "aud": "wrong-api", "exp": future_exp(),
            }),
            EC_KID,
        );
        assert!(matches!(v.verify(&token), Err(AuthError::InvalidToken(_))));
    }

    // The JWKS source derivation: explicit URL wins; else issuer-derived.
    #[test]
    fn jwks_source_precedence() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_auth_env();
        // No URL set → derive from issuer.
        assert_eq!(
            jwks_source(Some("https://auth.smoo.ai")),
            Some("https://auth.smoo.ai/.well-known/jwks.json".to_string())
        );
        assert_eq!(jwks_source(None), None);
        // Explicit URL wins over the issuer derivation.
        std::env::set_var("AUTH_JWT_JWKS_URL", "https://keys.example/jwks");
        assert_eq!(
            jwks_source(Some("https://auth.smoo.ai")),
            Some("https://keys.example/jwks".to_string())
        );
        clear_auth_env();
    }

    // AUTH_MODE=smoo with only an issuer (no static key) builds the JWKS-backed
    // verifier — the chart can drop AUTH_JWT_RS256_PUBLIC_KEY.
    #[test]
    fn from_env_smoo_with_issuer_only_builds_jwks() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_auth_env();
        std::env::set_var("AUTH_MODE", "smoo");
        std::env::set_var("AUTH_JWT_ISSUER", "https://auth.smoo.ai");
        let v = AuthConfig::from_env().expect("smoo builds from issuer alone");
        assert_eq!(v.mode(), "smoo");
        clear_auth_env();
    }

    // ---- Role ordering ---------------------------------------------------

    #[test]
    fn role_ordering_admin_ge_curator_ge_basic() {
        assert!(Role::Admin >= Role::Curator);
        assert!(Role::Curator >= Role::Basic);
        assert!(Role::Admin > Role::Basic);
        assert!(Role::Admin >= Role::Admin);
        // And the inverse never holds.
        assert!(Role::Basic < Role::Curator);
        assert!(Role::Curator < Role::Admin);
    }

    #[test]
    fn role_has_role_gate() {
        let admin = Principal::new("u", "o", Role::Admin, None);
        let basic = Principal::new("u", "o", Role::Basic, None);
        assert!(admin.has_role(Role::Curator));
        assert!(admin.has_role(Role::Basic));
        assert!(!basic.has_role(Role::Curator));
        assert!(basic.has_role(Role::Basic));
    }

    #[test]
    fn role_parse_known_and_unknown() {
        assert_eq!(Role::parse("admin").unwrap(), Role::Admin);
        assert_eq!(Role::parse("CURATOR").unwrap(), Role::Curator);
        assert_eq!(Role::parse(" basic ").unwrap(), Role::Basic);
        assert_eq!(Role::parse("user").unwrap(), Role::Basic);
        assert!(matches!(
            Role::parse("superuser"),
            Err(AuthError::MissingRole(_))
        ));
    }

    // ---- JwtVerifier round-trip ------------------------------------------

    #[test]
    fn jwt_verifier_round_trip_extracts_principal() {
        let verifier = JwtVerifier::hs256(SECRET, None, None);
        let token = sign(json!({
            "sub": "user-123",
            "org": "org-abc",
            "role": "curator",
            "name": "Ada Lovelace",
            "exp": future_exp(),
        }));
        let p = verifier.verify(&token).expect("verify");
        assert_eq!(p.user_id, "user-123");
        assert_eq!(p.org_id, "org-abc");
        assert_eq!(p.role, Role::Curator);
        assert_eq!(p.display_name.as_deref(), Some("Ada Lovelace"));
    }

    // ---- per-user conversation scope (SECURITY, pearl th-b2c60b) ----------

    #[test]
    fn jwt_verifier_extracts_the_email_claim() {
        // The `email` claim is the per-user conversation scope key — it must
        // come from the verified token, never from a client-supplied field.
        let verifier = JwtVerifier::hs256(SECRET, None, None);
        let token = sign(json!({
            "sub": "user-123",
            "org": "org-abc",
            "role": "basic",
            "email": "ada@example.com",
            "exp": future_exp(),
        }));
        let p = verifier.verify(&token).expect("verify");
        assert_eq!(p.email.as_deref(), Some("ada@example.com"));
    }

    #[test]
    fn a_token_without_a_usable_email_yields_no_scope_key() {
        // Absent OR blank ⇒ `None`, so the caller fails closed rather than
        // scoping to an empty-string "user" that could match participant rows
        // carrying an empty email.
        let verifier = JwtVerifier::hs256(SECRET, None, None);
        for claim in [json!(null), json!(""), json!("   ")] {
            let token = sign(json!({
                "sub": "user-123",
                "org": "org-abc",
                "role": "basic",
                "email": claim,
                "exp": future_exp(),
            }));
            let p = verifier.verify(&token).expect("verify");
            assert_eq!(p.email, None, "blank/absent email must not become a scope");
        }
    }

    #[test]
    fn only_the_identity_less_flavors_are_single_user() {
        // These three have no per-user identity concept, so conversation reads
        // stay unscoped (local daemon / dev). Everything else — including an
        // unknown future mode — is multi-user and must be scoped.
        for mode in ["none", "disabled", "local-token"] {
            assert!(is_single_user_mode(mode), "{mode} should be single-user");
        }
        for mode in ["jwt", "jwks", "smoo", "trusted", "some-future-mode", ""] {
            assert!(
                !is_single_user_mode(mode),
                "{mode} must be treated as multi-user (fail closed)"
            );
        }
    }

    #[test]
    fn with_email_treats_blank_as_absent() {
        let p = Principal::new("u", "o", Role::Basic, None).with_email("   ");
        assert_eq!(p.email, None);
        let p = Principal::new("u", "o", Role::Basic, None).with_email("a@b.com");
        assert_eq!(p.email.as_deref(), Some("a@b.com"));
    }

    #[test]
    fn jwt_verifier_parses_groups_claim_into_access_context() {
        // A token carrying a `groups` claim must surface those groups on the
        // Principal AND in the derived AccessContext — this is what lets a user
        // match a `github:owner/repo` document ACL on the chat retrieval path.
        let verifier = JwtVerifier::hs256(SECRET, None, None);
        let token = sign(json!({
            "sub": "user-7",
            "org": "org-x",
            "role": "basic",
            "groups": ["github:acme/secret", "eng"],
            "exp": future_exp(),
        }));
        let p = verifier.verify(&token).expect("verify");
        assert_eq!(p.groups, vec!["github:acme/secret", "eng"]);

        let ctx = p.access_context();
        assert_eq!(ctx.user_id.as_deref(), Some("user-7"));
        assert!(ctx.groups.contains(&"github:acme/secret".to_string()));
        // The principal's org is carried so a multi-tenant host adapter can scope
        // RAG to this tenant.
        assert_eq!(ctx.organization_id.as_deref(), Some("org-x"));
        // And it can read a doc scoped to one of its groups.
        let acl = crate::access_control::DocAcl::for_groups(["github:acme/secret"]);
        assert!(ctx.can_access(&acl), "group-scoped doc must be accessible");
    }

    #[test]
    fn jwt_verifier_no_groups_claim_yields_no_group_entitlements() {
        // No `groups` claim ⇒ empty groups ⇒ the principal cannot match a
        // group-scoped (private-repo) document.
        let verifier = JwtVerifier::hs256(SECRET, None, None);
        let token = sign(json!({
            "sub": "user-8", "org": "org-x", "role": "basic", "exp": future_exp(),
        }));
        let p = verifier.verify(&token).expect("verify");
        assert!(p.groups.is_empty());
        let acl = crate::access_control::DocAcl::for_groups(["github:acme/secret"]);
        assert!(
            !p.access_context().can_access(&acl),
            "LEAK: a principal with no groups must NOT read a group-scoped doc"
        );
    }

    #[test]
    fn jwt_verifier_accepts_org_id_alias() {
        let verifier = JwtVerifier::hs256(SECRET, None, None);
        let token = sign(json!({
            "sub": "u",
            "org_id": "org-from-alias",
            "role": "admin",
            "exp": future_exp(),
        }));
        let p = verifier.verify(&token).expect("verify");
        assert_eq!(p.org_id, "org-from-alias");
        assert_eq!(p.role, Role::Admin);
        assert!(p.display_name.is_none());
    }

    #[test]
    fn jwt_verifier_rejects_expired() {
        let verifier = JwtVerifier::hs256(SECRET, None, None);
        let token = sign(json!({
            "sub": "u",
            "org": "o",
            "role": "admin",
            "exp": (chrono::Utc::now() - chrono::Duration::hours(2)).timestamp(),
        }));
        let err = verifier.verify(&token).expect_err("must reject expired");
        assert!(matches!(err, AuthError::InvalidToken(_)));
    }

    #[test]
    fn jwt_verifier_rejects_wrong_secret() {
        let verifier = JwtVerifier::hs256(b"a-different-secret", None, None);
        let token = sign(json!({
            "sub": "u", "org": "o", "role": "admin", "exp": future_exp(),
        }));
        let err = verifier.verify(&token).expect_err("must reject bad sig");
        assert!(matches!(err, AuthError::InvalidToken(_)));
    }

    #[test]
    fn jwt_verifier_rejects_missing_role() {
        let verifier = JwtVerifier::hs256(SECRET, None, None);
        let token = sign(json!({
            "sub": "u", "org": "o", "exp": future_exp(),
        }));
        let err = verifier.verify(&token).expect_err("must reject no role");
        assert!(matches!(err, AuthError::MissingRole(_)));
    }

    #[test]
    fn jwt_verifier_rejects_unknown_role() {
        let verifier = JwtVerifier::hs256(SECRET, None, None);
        let token = sign(json!({
            "sub": "u", "org": "o", "role": "wizard", "exp": future_exp(),
        }));
        let err = verifier.verify(&token).expect_err("must reject bad role");
        assert!(matches!(err, AuthError::MissingRole(_)));
    }

    #[test]
    fn jwt_verifier_rejects_missing_org() {
        let verifier = JwtVerifier::hs256(SECRET, None, None);
        let token = sign(json!({
            "sub": "u", "role": "admin", "exp": future_exp(),
        }));
        let err = verifier.verify(&token).expect_err("must reject no org");
        assert!(matches!(err, AuthError::InvalidToken(_)));
    }

    #[test]
    fn jwt_verifier_rejects_empty_token() {
        let verifier = JwtVerifier::hs256(SECRET, None, None);
        assert_eq!(
            verifier.verify("   ").expect_err("empty"),
            AuthError::Unauthenticated
        );
    }

    #[test]
    fn jwt_verifier_rejects_garbage() {
        let verifier = JwtVerifier::hs256(SECRET, None, None);
        let err = verifier.verify("not.a.jwt").expect_err("garbage");
        assert!(matches!(err, AuthError::InvalidToken(_)));
    }

    #[test]
    fn jwt_verifier_enforces_audience_when_configured() {
        let verifier = JwtVerifier::hs256(SECRET, None, Some("expected-aud".to_string()));
        // Right audience → ok.
        let ok = sign(json!({
            "sub": "u", "org": "o", "role": "admin",
            "aud": "expected-aud", "exp": future_exp(),
        }));
        assert!(verifier.verify(&ok).is_ok());
        // Wrong audience → rejected.
        let bad = sign(json!({
            "sub": "u", "org": "o", "role": "admin",
            "aud": "other-aud", "exp": future_exp(),
        }));
        assert!(matches!(
            verifier.verify(&bad),
            Err(AuthError::InvalidToken(_))
        ));
    }

    // ---- SmooIdentityVerifier --------------------------------------------

    #[test]
    fn smoo_verifier_validates_issuer_keyed_token() {
        let verifier =
            SmooIdentityVerifier::hs256(SECRET, "https://auth.smoo.ai".to_string(), None);
        let token = sign(json!({
            "sub": "u", "org": "o", "role": "admin",
            "iss": "https://auth.smoo.ai", "exp": future_exp(),
        }));
        let p = verifier.verify(&token).expect("verify");
        assert_eq!(p.role, Role::Admin);
        assert_eq!(verifier.mode(), "smoo");
    }

    #[test]
    fn smoo_verifier_rejects_wrong_issuer() {
        let verifier =
            SmooIdentityVerifier::hs256(SECRET, "https://auth.smoo.ai".to_string(), None);
        let token = sign(json!({
            "sub": "u", "org": "o", "role": "admin",
            "iss": "https://evil.example", "exp": future_exp(),
        }));
        assert!(matches!(
            verifier.verify(&token),
            Err(AuthError::InvalidToken(_))
        ));
    }

    #[test]
    fn smoo_introspect_is_stubbed_misconfigured() {
        let verifier =
            SmooIdentityVerifier::hs256(SECRET, "https://auth.smoo.ai".to_string(), None);
        assert!(matches!(
            verifier.introspect("opaque-token"),
            Err(AuthError::Misconfigured(_))
        ));
    }

    // ---- NoAuthVerifier --------------------------------------------------

    #[test]
    fn no_auth_returns_fixed_admin() {
        let verifier = NoAuthVerifier::new("dev-org");
        let p = verifier.verify("anything-or-nothing").expect("no-auth");
        assert_eq!(p.role, Role::Admin);
        assert_eq!(p.org_id, "dev-org");
        assert_eq!(verifier.mode(), "none");
    }

    // ---- LocalTokenVerifier ----------------------------------------------

    #[test]
    fn local_token_accepts_exact_secret_as_local_admin() {
        let v = LocalTokenVerifier::new("s3cret-local");
        let p = v.verify("s3cret-local").expect("matching token");
        assert_eq!(p.role, Role::Admin);
        assert_eq!(p.user_id, "local");
        assert_eq!(p.org_id, "local");
        assert_eq!(v.mode(), "local-token");
    }

    #[test]
    fn local_token_fails_closed_on_wrong_or_empty() {
        let v = LocalTokenVerifier::new("s3cret-local");
        assert!(matches!(v.verify(""), Err(AuthError::Unauthenticated)));
        assert!(matches!(v.verify("nope"), Err(AuthError::InvalidToken(_))));
        assert!(matches!(
            v.verify("s3cret"),
            Err(AuthError::InvalidToken(_))
        ));
    }

    // ---- AuthConfig::from_env — secure by default ------------------------
    //
    // These mutate process env, so they run serially under a shared lock to
    // avoid cross-test interference.

    use std::sync::Mutex;
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn clear_auth_env() {
        for k in [
            "AUTH_MODE",
            "AUTH_JWT_HS256_SECRET",
            "AUTH_JWT_RS256_PUBLIC_KEY",
            "AUTH_JWT_JWKS_URL",
            "AUTH_JWT_ISSUER",
            "AUTH_JWT_AUDIENCE",
            "AUTH_DEV_ORG_ID",
        ] {
            std::env::remove_var(k);
        }
    }

    #[test]
    fn from_env_default_disables_admin_without_key() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_auth_env();
        // No AUTH_MODE, no key → the server BOOTS (so /ws serves) with the admin
        // API disabled — it does NOT silently fall back to no-auth, and it does
        // NOT hard-fail the whole service.
        let v = AuthConfig::from_env().expect("default boots with admin disabled");
        assert_eq!(v.mode(), "disabled");
        // Every admin request is rejected until auth is configured.
        assert!(matches!(
            v.verify("anything"),
            Err(AuthError::InvalidToken(_))
        ));
        clear_auth_env();
    }

    #[test]
    fn from_env_explicit_jwt_without_key_hard_errors() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_auth_env();
        // EXPLICITLY asking for jwt with no key is a loud startup error (an
        // operator who set AUTH_MODE=jwt and forgot the key must be told).
        std::env::set_var("AUTH_MODE", "jwt");
        match AuthConfig::from_env() {
            Err(AuthError::Misconfigured(_)) => {}
            Ok(_) => panic!("explicit keyless jwt must NOT fall back to disabled/no-auth"),
            Err(other) => panic!("expected Misconfigured, got {other}"),
        }
        clear_auth_env();
    }

    #[test]
    fn from_env_jwt_with_hs256_secret_builds() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_auth_env();
        std::env::set_var("AUTH_MODE", "jwt");
        std::env::set_var("AUTH_JWT_HS256_SECRET", "shhh");
        let v = AuthConfig::from_env().expect("builds");
        assert_eq!(v.mode(), "jwt");
        clear_auth_env();
    }

    #[test]
    fn from_env_none_only_when_explicit() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_auth_env();
        std::env::set_var("AUTH_MODE", "none");
        std::env::set_var("AUTH_DEV_ORG_ID", "explicit-dev-org");
        let v = AuthConfig::from_env().expect("none builds");
        assert_eq!(v.mode(), "none");
        let p = v.verify("").expect("no-auth principal");
        assert_eq!(p.role, Role::Admin);
        assert_eq!(p.org_id, "explicit-dev-org");
        clear_auth_env();
    }

    #[test]
    fn from_env_unknown_mode_errors() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_auth_env();
        std::env::set_var("AUTH_MODE", "banana");
        assert!(matches!(
            AuthConfig::from_env(),
            Err(AuthError::Misconfigured(_))
        ));
        clear_auth_env();
    }

    // ---- TrustedIdentityVerifier (AUTH_MODE=trusted) ---------------------
    //
    // Tokenless proxied-integration mode: the upstream forwards identity as a
    // base64url(JSON) blob in the same slot a token would ride. NO signature,
    // NO exp; reuses the Claims→Principal mapping. MUST fail closed (error →
    // anonymous at the connect path), NEVER admin, on bad input.

    /// Encode a claims object as the `base64url(JSON)` blob the trusted upstream
    /// would forward (unpadded URL-safe — the canonical form).
    fn forward(claims: serde_json::Value) -> String {
        use base64::Engine as _;
        let json = serde_json::to_vec(&claims).expect("serialize claims");
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json)
    }

    #[test]
    fn trusted_verifier_parses_forwarded_identity_into_principal_with_groups() {
        let verifier = TrustedIdentityVerifier::new();
        // No `exp` here on purpose — the upstream owns lifetime; trusted mode
        // must NOT require it.
        let blob = forward(json!({
            "sub": "user-42",
            "org": "acme",
            "role": "curator",
            "name": "Grace Hopper",
            "groups": ["github:acme/secret", "eng"],
        }));
        let p = verifier.verify(&blob).expect("trusted verify");
        assert_eq!(p.user_id, "user-42");
        assert_eq!(p.org_id, "acme");
        assert_eq!(p.role, Role::Curator);
        assert_eq!(p.display_name.as_deref(), Some("Grace Hopper"));
        assert_eq!(p.groups, vec!["github:acme/secret", "eng"]);
        assert_eq!(verifier.mode(), "trusted");

        // The groups carry into the AccessContext so the SAME ACL enforcement a
        // JWT drives applies to a forwarded identity.
        let ctx = p.access_context();
        let acl = crate::access_control::DocAcl::for_groups(["github:acme/secret"]);
        assert!(
            ctx.can_access(&acl),
            "forwarded group must drive ACL access"
        );
    }

    #[test]
    fn trusted_verifier_accepts_org_id_alias_and_padded_base64() {
        use base64::Engine as _;
        let verifier = TrustedIdentityVerifier::new();
        // `org_id` alias + PADDED url-safe base64 must both be accepted.
        let json = serde_json::to_vec(&json!({
            "sub": "u", "org_id": "org-alias", "role": "admin",
        }))
        .unwrap();
        let blob = base64::engine::general_purpose::URL_SAFE.encode(json);
        let p = verifier.verify(&blob).expect("padded + alias");
        assert_eq!(p.org_id, "org-alias");
        assert_eq!(p.role, Role::Admin);
    }

    #[test]
    fn trusted_verifier_empty_is_unauthenticated_not_admin() {
        let verifier = TrustedIdentityVerifier::new();
        // Absent/empty forwarded identity ⇒ Unauthenticated error (which the
        // connect path maps to anonymous), NOT a fabricated admin principal.
        assert_eq!(
            verifier.verify("   ").expect_err("empty must error"),
            AuthError::Unauthenticated
        );
    }

    #[test]
    fn trusted_verifier_malformed_base64_errors_never_admin() {
        let verifier = TrustedIdentityVerifier::new();
        let err = verifier
            .verify("!!!not base64!!!")
            .expect_err("malformed base64 must error");
        assert!(matches!(err, AuthError::InvalidToken(_)));
    }

    #[test]
    fn trusted_verifier_malformed_json_errors_never_admin() {
        use base64::Engine as _;
        let verifier = TrustedIdentityVerifier::new();
        // Valid base64url but the bytes aren't claims JSON.
        let blob = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"not json at all");
        let err = verifier.verify(&blob).expect_err("non-json must error");
        assert!(matches!(err, AuthError::InvalidToken(_)));
    }

    #[test]
    fn trusted_verifier_missing_role_errors_never_admin() {
        // A forwarded identity with NO role must NOT silently become admin — it
        // is a MissingRole error (→ anonymous at the connect path).
        let verifier = TrustedIdentityVerifier::new();
        let blob = forward(json!({ "sub": "u", "org": "o" }));
        let err = verifier.verify(&blob).expect_err("no role must error");
        assert!(matches!(err, AuthError::MissingRole(_)));
    }

    #[test]
    fn trusted_verifier_missing_org_errors_never_admin() {
        let verifier = TrustedIdentityVerifier::new();
        let blob = forward(json!({ "sub": "u", "role": "admin" }));
        let err = verifier.verify(&blob).expect_err("no org must error");
        assert!(matches!(err, AuthError::InvalidToken(_)));
    }

    #[test]
    fn from_env_trusted_only_when_explicit() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_auth_env();
        // trusted is reached ONLY by explicit AUTH_MODE=trusted — no key needed
        // (there is nothing to verify), and it never requires AUTH_JWT_* config.
        std::env::set_var("AUTH_MODE", "trusted");
        let v = AuthConfig::from_env().expect("trusted builds");
        assert_eq!(v.mode(), "trusted");
        // A forwarded identity is honored...
        let blob = forward(json!({ "sub": "u", "org": "o", "role": "basic" }));
        assert_eq!(
            v.verify(&blob).expect("trusted principal").role,
            Role::Basic
        );
        // ...and bad input is an error (→ anonymous at the connect path), never admin.
        assert!(v.verify("garbage").is_err());
        clear_auth_env();
    }

    #[test]
    fn from_env_unset_does_not_select_trusted() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_auth_env();
        // Secure-by-default unset case is UNCHANGED: no AUTH_MODE ⇒ admin-disabled,
        // NOT trusted. trusted is only ever reached by an explicit opt-in.
        let v = AuthConfig::from_env().expect("default boots");
        assert_eq!(v.mode(), "disabled");
        assert_ne!(v.mode(), "trusted");
        clear_auth_env();
    }

    #[test]
    fn from_env_smoo_requires_issuer() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_auth_env();
        std::env::set_var("AUTH_MODE", "smoo");
        // No issuer → misconfig (there is nothing to key the JWKS or validate
        // `iss` against).
        assert!(matches!(
            AuthConfig::from_env(),
            Err(AuthError::Misconfigured(_))
        ));
        // Issuer alone now builds the JWKS-backed verifier (no static key
        // required — this is the ES256 path for auth.smoo.ai).
        std::env::set_var("AUTH_JWT_ISSUER", "https://auth.smoo.ai");
        let v = AuthConfig::from_env().expect("smoo builds from issuer (JWKS)");
        assert_eq!(v.mode(), "smoo");
        // A static key still works and takes precedence over JWKS.
        std::env::set_var("AUTH_JWT_HS256_SECRET", "shhh");
        let v = AuthConfig::from_env().expect("smoo builds with static key");
        assert_eq!(v.mode(), "smoo");
        clear_auth_env();
    }
}
