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

use jsonwebtoken::{decode, Algorithm, DecodingKey, Validation};
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
            groups: Vec::new(),
        }
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
    /// knowledge-retrieval ACL layer. Both the user id **and** the principal's
    /// groups carry through, so a retrieval as this principal can match a
    /// document scoped to the user *or* to any group the principal belongs to
    /// (the JWT `groups` claim — see [`Claims`]).
    #[must_use]
    pub fn access_context(&self) -> AccessContext {
        AccessContext::new(Some(self.user_id.clone()), self.groups.clone())
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

/// Validates a JWT and extracts a [`Principal`]. The **BYO** path: SST OpenAuth
/// (or any OIDC IdP) issues the token; this verifies signature + standard claims
/// and maps `sub`→`user_id`, `org`/`org_id`→`org_id`, `role`→[`Role`],
/// `name`→`display_name`.
pub struct JwtVerifier {
    key: VerifyKey,
    validation: Validation,
}

impl JwtVerifier {
    /// An HS256 verifier over a shared secret. Optionally constrains `iss`/`aud`.
    #[must_use]
    pub fn hs256(secret: &[u8], issuer: Option<String>, audience: Option<String>) -> Self {
        let mut validation = Validation::new(Algorithm::HS256);
        configure_validation(&mut validation, issuer, audience);
        Self {
            key: VerifyKey::Hs256(Box::new(DecodingKey::from_secret(secret))),
            validation,
        }
    }

    /// An RS256 verifier over a PEM-encoded public key. Optionally constrains
    /// `iss`/`aud`. (Structural RS256 support — a JWKS-url variant would fetch +
    /// cache keys; see [`AuthConfig`].)
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
            key: VerifyKey::Rs256(Box::new(key)),
            validation,
        })
    }

    /// Decode + validate, returning the [`Principal`]. Shared by
    /// [`SmooIdentityVerifier`].
    fn decode_principal(&self, token: &str) -> Result<Principal, AuthError> {
        if token.trim().is_empty() {
            return Err(AuthError::Unauthenticated);
        }
        let key = match &self.key {
            VerifyKey::Hs256(k) | VerifyKey::Rs256(k) => k.as_ref(),
        };
        let data = decode::<Claims>(token, key, &self.validation)
            .map_err(|e| AuthError::InvalidToken(e.to_string()))?;
        data.claims.into_principal()
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
/// | `AUTH_JWT_RS256_PUBLIC_KEY` | — | RS256 PEM public key (takes precedence over HS256). |
/// | `AUTH_JWT_ISSUER` | — | Required `iss` (optional). |
/// | `AUTH_JWT_AUDIENCE` | — | Required `aud` (optional). |
/// | `AUTH_DEV_ORG_ID` | `dev-org` | Org id for the `none`-mode admin principal. |
///
/// **Explicitly** setting `AUTH_MODE=jwt`/`smoo` with **no** key is a hard
/// [`AuthError::Misconfigured`] error — not a silent fall-through to no-auth.
/// Leaving `AUTH_MODE` **unset** with no key boots the server with the admin API
/// **disabled** ([`AdminDisabledVerifier`]) so `/ws` serves without forcing auth
/// config; `/admin` then returns 401 until configured (or `AUTH_MODE=none` for dev).

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
                    Err(AuthError::Misconfigured(
                        "AUTH_MODE=smoo requires AUTH_JWT_RS256_PUBLIC_KEY or AUTH_JWT_HS256_SECRET"
                            .to_string(),
                    ))
                }
            }
            other => Err(AuthError::Misconfigured(format!(
                "unknown AUTH_MODE '{other}' (expected jwt | smoo | trusted | none)"
            ))),
        }
    }

    /// Build a [`JwtVerifier`] from env, preferring RS256 (PEM) over HS256.
    fn build_jwt(
        issuer: Option<String>,
        audience: Option<String>,
    ) -> Result<JwtVerifier, AuthError> {
        if let Some(pem) = env_nonempty("AUTH_JWT_RS256_PUBLIC_KEY") {
            JwtVerifier::rs256(pem.as_bytes(), issuer, audience)
        } else if let Some(secret) = env_nonempty("AUTH_JWT_HS256_SECRET") {
            Ok(JwtVerifier::hs256(secret.as_bytes(), issuer, audience))
        } else {
            Err(AuthError::Misconfigured(
                "AUTH_MODE=jwt requires AUTH_JWT_RS256_PUBLIC_KEY or AUTH_JWT_HS256_SECRET \
                 (refusing to fall back to no-auth)"
                    .to_string(),
            ))
        }
    }
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
    fn from_env_smoo_requires_issuer_and_key() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_auth_env();
        std::env::set_var("AUTH_MODE", "smoo");
        // No issuer → misconfig.
        assert!(matches!(
            AuthConfig::from_env(),
            Err(AuthError::Misconfigured(_))
        ));
        std::env::set_var("AUTH_JWT_ISSUER", "https://auth.smoo.ai");
        // Issuer but no key → misconfig.
        assert!(matches!(
            AuthConfig::from_env(),
            Err(AuthError::Misconfigured(_))
        ));
        std::env::set_var("AUTH_JWT_HS256_SECRET", "shhh");
        let v = AuthConfig::from_env().expect("smoo builds");
        assert_eq!(v.mode(), "smoo");
        clear_auth_env();
    }
}
