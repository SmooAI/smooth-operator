# Admin API + Auth/RBAC

The admin HTTP API is the auth-gated backend the **management console** (a
Next.js app, Phase 12 increment 2) consumes: whoami, chat history, indexing
status, and document sets. It is mounted alongside the existing `/ws` WebSocket
endpoint on the same `smooth-operator-server` axum service, so one process serves
both the realtime chat protocol and the management surface.

This page covers the **auth model** (Role / Principal / AuthVerifier, the three
`AUTH_MODE`s, and secure-by-default), the **admin endpoints** and their role
gates, and how **org-scoping** and **"Basic sees own"** work.

- Auth/RBAC core: [`rust/smooth-operator/src/auth.rs`](../rust/smooth-operator/src/auth.rs)
- Admin routes + extractor (read **and** write): [`rust/smooth-operator-server/src/admin.rs`](../rust/smooth-operator-server/src/admin.rs)
- Connector-config store + `auth_ref`: [`rust/smooth-operator/src/connector_config.rs`](../rust/smooth-operator/src/connector_config.rs)
- Agent-settings store: [`rust/smooth-operator/src/settings.rs`](../rust/smooth-operator/src/settings.rs)
- State wiring: [`rust/smooth-operator-server/src/state.rs`](../rust/smooth-operator-server/src/state.rs)
- Related: [ACCESS-CONTROL.md](ACCESS-CONTROL.md) (document-level ACL — RBAC sits on top), [INDEXING.md](INDEXING.md), [DOCUMENT-SETS.md](DOCUMENT-SETS.md)

---

## Auth model

### Role

A total order so a route can gate on a **minimum** role (`principal.role >= min`):

```
Admin  >=  Curator  >=  Basic
```

| Role | Meaning |
| --- | --- |
| **Admin** | Full org-wide read of chat history, indexing, document sets (and future write/config). |
| **Curator** | Org-wide read of chat history + curation surfaces (indexing, document sets). The knowledge-curation persona. |
| **Basic** | An end user. May see only their **own** conversations. |

### Principal

The authenticated identity a request runs as. Everything the admin API reads is
scoped to `org_id`; `role` gates which operations are allowed and whether reads
are org-wide or self-only.

```rust
pub struct Principal {
    pub user_id: String,         // JWT `sub`
    pub org_id:  String,         // JWT `org` (or `org_id` alias)
    pub role:    Role,           // JWT `role`
    pub display_name: Option<String>, // JWT `name`
}
```

A `Principal` maps to the document-level
[`AccessContext`](ACCESS-CONTROL.md) (`Principal::access_context()`) so the same
identity drives both RBAC (which operations) and document ACL (which documents).

### AuthVerifier — the one seam

```rust
pub trait AuthVerifier: Send + Sync {
    fn verify(&self, bearer_token: &str) -> Result<Principal, AuthError>;
    fn mode(&self) -> &'static str;
}
```

Three implementations cover the deployment shapes:

| Verifier | `AUTH_MODE` | Path | What it does |
| --- | --- | --- | --- |
| **`JwtVerifier`** | `jwt` | **BYO** | Validates a JWT issued by the customer's own IdP. **SST OpenAuth** (`@openauthjs/openauth` + `sst.aws.Auth`; OIDC/OAuth/password, SAML via OIDC bridge) issues exactly these. **HS256** (shared secret) and **RS256** (PEM public key) supported. Extracts `sub`→`user_id`, `org`/`org_id`→`org_id`, `role`→`Role`, `name`→`display_name`. |
| **`SmooIdentityVerifier`** | `smoo` | **Hosted** | Validates a **Smoo-issued** JWT keyed to Smoo's issuer/audience — `lom.smoo.ai` wires Smoo's identity. Reuses `JwtVerifier` internals (Smoo signs a JWT; we verify it locally with Smoo's public key / shared secret — no per-request network call). The opaque-token **live introspection** (RFC 7662) variant is documented + stubbed (`introspect()` returns `Misconfigured`) because it needs a network call to `{auth_server}/introspect`. |
| **`NoAuthVerifier`** | `none` | **Dev only** | Returns a fixed `Admin` principal for any (or no) token. Reachable **only** via an explicit `AUTH_MODE=none`. |

### BYO (SST OpenAuth) vs Smoo-identity duality

There are two ways to authenticate, and the service supports both via the
`AUTH_MODE` switch:

- **BYO** (`jwt`) — the customer brings their own IdP. The recommended self-host
  path is **SST OpenAuth** (`sst.aws.Auth` issuing OpenAuth JWTs), but any OIDC
  IdP that emits a JWT with `sub` / `org` / `role` claims works. The service only
  needs the verification key (HS256 secret or RS256 public key) and optionally an
  `iss` / `aud` to constrain.
- **Hosted** (`smoo`) — Smoo's identity issues the token; `lom.smoo.ai` (the
  managed offering) wires this. Same JWT validation, keyed to Smoo's issuer.

### Secure-by-default

`AuthConfig::from_env()` selects the verifier:

| Env var | Default | Meaning |
| --- | --- | --- |
| `AUTH_MODE` | `jwt` | `jwt` (BYO) \| `smoo` (hosted) \| `none` (dev only). |
| `AUTH_JWT_HS256_SECRET` | — | HS256 shared secret. |
| `AUTH_JWT_RS256_PUBLIC_KEY` | — | RS256 PEM public key (takes precedence over HS256). |
| `AUTH_JWT_ISSUER` | — | Required `iss` (optional; **required** for `smoo`). |
| `AUTH_JWT_AUDIENCE` | — | Required `aud` (optional). |
| `AUTH_DEV_ORG_ID` | `dev-org` | Org id for the `none`-mode admin principal. |

The default is **`jwt`**, and `jwt` / `smoo` with **no key configured** is a hard
`AuthError::Misconfigured` — the server **refuses to start** rather than silently
falling back to no-auth. The no-auth verifier is reachable **only** when
`AUTH_MODE=none` is set explicitly, so it can never be the silent production
default. The binary wires this via `build_state_from_env` (in
[`server.rs`](../rust/smooth-operator-server/src/server.rs)); `bind()` propagates
the misconfig error so a bad config fails the boot.

Keys are read from env (or `@smooai/config` when deployed) and **never logged**.

---

## Admin endpoints

All routes are mounted under `/admin`. JSON in, JSON out. Auth failures return
the protocol's `error` envelope (`{ code, message }`) with the matching HTTP
status (401 unauthenticated / invalid token / missing role; 403 insufficient
role; 404 cross-org / unknown).

| Method + path | Min role | Returns |
| --- | --- | --- |
| `GET /admin/health` | — (public) | `{ "status": "ok" }` — liveness, no auth. |
| `GET /admin/me` | Basic | The caller's `Principal`. |
| `GET /admin/conversations?limit&cursor` | Basic | Org-scoped chat history. Admin/Curator: org-wide; Basic: own only. Offset-paged (`cursor` = start index, `nextCursor` when more). |
| `GET /admin/conversations/{id}/messages` | Basic | Messages for one conversation (role-scoped — a Basic caller must own it). |
| `GET /admin/indexing/runs` | Curator | Indexing-run status across the org's connectors (from the `IndexingStore`). |
| `GET /admin/document-sets` | Curator | Distinct document-set names + doc counts. |
| `GET /admin/connectors` | Curator | List this org's connector configs. |
| `POST /admin/connectors` | **Admin** | Create a connector config (returns `201` + the created connector). |
| `GET /admin/connectors/{id}` | Curator | One connector config (org-scoped; cross-org/unknown ⇒ `404`). |
| `PUT /admin/connectors/{id}` | **Admin** | Update a connector config (id + `createdAt` preserved). |
| `DELETE /admin/connectors/{id}` | **Admin** | Delete a connector config (`204`; cross-org/unknown ⇒ `404`). |
| `POST /admin/connectors/{id}/index` | Curator | Build the connector from its config and run one indexing pass; returns the `IndexingRun` (also visible in `/admin/indexing/runs`). |
| `GET /admin/settings` | Curator | The org's agent settings (model, system prompt, default tools) — defaults if unset. |
| `PUT /admin/settings` | **Admin** | Replace the org's agent settings. |

The **write** routes (Phase 12, increment 3) follow the same `RequireRole<MIN>`
gating: **read** surfaces (`GET /admin/connectors`, `/{id}`, `/admin/settings`)
are **Curator**; **mutations** (`POST`/`PUT`/`DELETE` connectors, `PUT` settings)
are **Admin-only**; the **index trigger** is **Curator** (curation is a Curator
responsibility). Everything is scoped to `principal.org_id` — a cross-org id is a
`404`, never `403`. Unknown connector `kind`s and malformed `config` payloads are
rejected with a `400` `VALIDATION_ERROR` before anything is stored.

### Connector config + the `auth_ref` secret model

A **connector config** is the persisted, org-scoped description of one source the
indexing loop pulls from:

```jsonc
{
  "id": "uuid",
  "name": "Docs repo",          // human label; the indexing-run is keyed by this
  "kind": "github",             // github | web | file (unknown ⇒ 400)
  "config": {                   // kind-specific, free-form payload
    "owner": "smooai", "repo": "docs",
    "ref": "main", "visibility": "private",
    "auth_ref": "GITHUB_TOKEN"  // a SECRET NAME — never the token itself
  },
  "enabled": true,
  "createdAt": "…", "updatedAt": "…"
}
```

**`auth_ref` is the secret model.** The config never stores a credential — only the
**name** of an environment variable / secret (e.g. `"GITHUB_TOKEN"`). The actual
token is resolved from env (or `@smooai/config` when deployed) **at index time**,
used to build the live connector, and discarded. It is never persisted in the
store and **never returned in any API response** — a `GET` (single or list) echoes
the `auth_ref` *name* but no token value.

Required `config` fields per kind (enforced with a `400` on create/update):

| `kind` | required | optional |
| --- | --- | --- |
| `github` | `owner`, `repo` | `ref`, `visibility` (`public`/`private`), `auth_ref` |
| `web` | `url` | — |
| `file` | `path` | — |

### The index-trigger flow (`POST /admin/connectors/{id}/index`)

1. Load the org-scoped connector config (`404` if absent / cross-org).
2. **Build the live connector** from its `config` (`build_connector`): `web` →
   `WebConnector`, `file` → `FileConnector`, `github` → `GithubConnector`. For
   `github`, the token is resolved from `auth_ref` → env **at this moment**:
   - `auth_ref` set + env present ⇒ `GithubAuth::Token`.
   - `auth_ref` set but env **missing/empty** ⇒ a clean **`400` `VALIDATION_ERROR`**
     (no panic, no GitHub call).
   - no `auth_ref`: a **public** repo indexes unauthenticated; a **private** repo
     is a `400` (a private repo needs a credential).
   The built connector's `name()` is overridden to the configured connector name so
   the run is keyed by the human label.
3. Run `IndexingService::run_once(connector, indexing_store, chunker, embedder,
   knowledge)` — the same incremental loop documented in [INDEXING.md](INDEXING.md)
   (`latest_cursor` → `pull(since)` → chunk → embed → store). The chunker/embedder
   are the network-free defaults (`Chunker::default()`, `DeterministicEmbedder`).
4. The resulting `IndexingRun` is recorded in the **shared `IndexingStore`** (so it
   also shows in `GET /admin/indexing/runs`) and returned under a `run` key.

### Auth extractor — `require_role`

`require_role(min)` is realized as the `RequireRole<MIN>` axum extractor
(`MIN` is a const role rank: `0 = Basic`, `1 = Curator`, `2 = Admin`). It reads
`Authorization: Bearer <token>`, verifies it via the configured `AuthVerifier`,
and rejects with 401/403 **before** the handler body runs. A handler that needs
Curator simply takes `RequireRole<1>` as an argument.

### Example

```bash
# Liveness — no auth.
curl -s https://host/admin/health
# {"status":"ok"}

# Whoami — any authenticated role.
curl -s -H "Authorization: Bearer $JWT" https://host/admin/me
# {"userId":"alice","orgId":"org-acme","role":"curator","displayName":"Ada"}

# Chat history — org-scoped, role-filtered.
curl -s -H "Authorization: Bearer $JWT" "https://host/admin/conversations?limit=50"
```

---

## Org-scoping + "Basic sees own"

Every read filters to `principal.org_id` (via the storage adapter's
`list_conversations_by_org`). Multi-tenancy is enforced at the data layer:

- **Admin / Curator** see the whole org.
- **Basic** sees only conversations they **own** — a conversation is owned when
  one of its `User` participants carries `external_id == principal.user_id`. The
  list is filtered to owned conversations; `/messages` returns **403** for a
  conversation a Basic caller doesn't own.
- A conversation in a **different org** returns **404** (existence is not leaked
  across orgs), never 403.

This mirrors the document-level [`AccessContext`](ACCESS-CONTROL.md) model RBAC
sits on top of: RBAC gates *which admin operations*; `AccessContext` gates *which
documents* a retrieval returns.

---

## Wiring + state

`AppState` (in [`state.rs`](../rust/smooth-operator-server/src/state.rs)) carries,
alongside the storage adapter and config:

- `auth: Arc<dyn AuthVerifier>` — the env-selected verifier.
- `indexing: Arc<dyn IndexingStore>` — an `InMemoryIndexingStore` for now;
  the persistent Postgres/DynamoDB store is the follow-up (see [INDEXING.md](INDEXING.md)).
- `connector_configs: Arc<dyn ConnectorConfigStore>` — an
  `InMemoryConnectorConfigStore` (Phase 12 increment 3). CRUD'd by the
  `/admin/connectors` write API, org-scoped, holds an `auth_ref` (secret **name**)
  not a credential. The persistent follow-up is a Postgres/DynamoDB
  `connector_configs` table keyed on `(org_id, id)`.
- `settings: Arc<dyn SettingsStore>` — an `InMemorySettingsStore` (per-org agent
  settings: model / system prompt / default tools), read/written by
  `/admin/settings`. Persistent follow-up is an `agent_settings` table keyed on
  `org_id`.
- a **document-set registry** (set name → doc count) — the in-memory knowledge
  backend drops document metadata on ingest, so `/admin/document-sets` reads set
  names + counts from this side registry, populated as docs are seeded/ingested.

The `/ws` route, ACL, citations, and curation are unchanged — the admin router is
merged into the same axum app.

---

## Next: the management console (increment 2)

The Next.js management console (Phase 12 increment 2) consumes this API:
connector config (the increment-3 write endpoints above), document sets, chat
history, indexing status, and settings. It authenticates with the same JWT (BYO
SST OpenAuth or Smoo identity) and calls these endpoints with the user's bearer
token, so the console inherits the same RBAC gates and org-scoping enforced here.
The console pages themselves are a separate increment; the backend write surface
they drive is complete.
