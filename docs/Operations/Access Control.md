# Document-level access control

Mature knowledge platforms sync per-connector permissions and filter retrieval by user entitlement;
before this, smooth-operator filtered knowledge by `organizationId` only.
This is the within-org **document-level** layer (feature gap **G3**, the
highest-severity gap in [[Feature Gaps]]):
even inside one organization, a document may be restricted to specific users or
groups, and a retrieval must only ever return documents the requester is
entitled to read.

Org isolation is unchanged and still happens upstream (the Postgres knowledge
base filters on `organizationId`, DynamoDB scopes per-org indexes). Access
control is **additive on top** of org isolation.

## Where enforcement lives — our layer, not the engine

smooth-operator-core's `KnowledgeBase` trait is upstream and read-only to this repo.
Two facts force enforcement into our layer:

1. `KnowledgeBase::query` returns a `KnowledgeResult` that carries only
   `document_id` / `chunk` / `score` / `source` — **not** the stored metadata.
2. The in-memory backend drops document metadata on ingest entirely; the
   Postgres backend stores it but doesn't return it from `query`.

So we cannot read an ACL back out of a query result. Instead, the
`AclKnowledgeStore` (in `smooth-operator`, `src/access_control.rs`)
wraps any inner `KnowledgeBase` and:

- **records the document → ACL mapping at ingest** into a side table it owns
  (parsed from the document metadata), forwarding the document unchanged to the
  inner backend; then
- **filters at read**: a per-requester reader over-fetches from the inner
  backend, looks each result's ACL up in the side table, and drops any the
  requester cannot access.

This wrapper is the **in-memory** enforcement path. Its ACL side table is
process-local, so it cannot carry a document's ACL from the ingestion process to
a separate serving process. For the durable backends the ACL is therefore
**persisted with the document** and enforced from storage at read (see
[Durable persistence](#durable-persistence-postgres--dynamodb) below) — so the
guarantee survives the ingest→serve boundary, not just a single process.

## The `StorageAdapter` ACL seam — `knowledge_for_access`

Every backend exposes two knowledge handles through the `StorageAdapter` trait
(`smooth-operator/src/adapter.rs`):

- **`knowledge()`** — org isolation only. Used by ingest / admin / seeding. It
  does **not** enforce within-org ACLs.
- **`knowledge_for_access(&AccessContext)`** — an **ACL-enforcing** handle bound
  to the requester. Its `query` returns only documents the requester is entitled
  to read. **This is the handle the chat retrieval path MUST use.**

Per backend:

| Backend   | `knowledge_for_access` enforcement |
| --------- | ---------------------------------- |
| In-memory | Wraps the shared `AclKnowledgeStore` reader (side table populated at ingest). |
| Postgres  | A `PgKnowledgeBase` clone bound to the `AccessContext`; filters in SQL against the stored `acl` column (a restricted row is never even fetched). |
| DynamoDB  | A `DynamoKnowledgeBase` clone bound to the `AccessContext`; post-filters the brute-force scan against each item's stored `acl` attribute. |

The default trait impl wraps `knowledge()` in an `AclKnowledgeStore` reader with
an empty side table (every doc treated as org-public — the raw `knowledge()`
behavior, not a regression). The three real backends override it to enforce
durably.

## Enforcement on the live chat path (server + lambda)

> This closed the **#1 adversarial-review security finding**: the ACL layer was
> dead on the live chat path, so a private GitHub repo was retrievable by **any**
> chat user. The runner queried `storage.knowledge()` **raw** — no
> `AccessContext`, no ACL reader — for both the auto-injected context and the
> `knowledge_search` tool.

The streaming chat runner (`smooth-operator-server/src/runner.rs`,
`run_streaming_turn`) — used by **both** the reference WS server
(`handler.rs`) and the production AWS Lambda (`smooth-operator-lambda/src/dispatch.rs`) —
now takes an `AccessContext` on its `TurnRequest` and builds **one**
`storage.knowledge_for_access(&access)` handle that feeds **both** retrieval
surfaces:

1. the engine's auto-injected `[Relevant knowledge]` context, and
2. the agent's `knowledge_search` tool.

A restricted document is dropped before it can reach the model **or** a citation.

### `/ws` authentication → `AccessContext`

- **Reference server**: the bearer JWT rides on the `?token=` query param of the
  `/ws` upgrade (browsers can't set custom headers on a WebSocket handshake). It
  is verified once at connect via the configured `AuthVerifier`, mapped to the
  `Principal`'s `AccessContext`, and threaded into every turn on that connection.
- **Lambda**: API Gateway WebSocket has no persistent socket, so the token rides
  on the `send_message` frame (a `token` field), verified per frame.

**Fail closed for ACL'd content.** When no token is presented, the verifier is
unconfigured/disabled (dev/no-auth), or the token fails to verify, the connection
runs as `AccessContext::anonymous()` — which sees **only org-public** knowledge,
**not** every document. Verification failures are logged (never the token) and
degrade to anonymous rather than dropping the connection, so the dev/no-auth case
still serves org-public knowledge.

### Groups come from the JWT

`Principal::access_context()` now populates **both** the user id and the
principal's **groups**, parsed from a `groups` claim on the JWT (`auth.rs`,
`Claims.groups`). This is what lets an authenticated user match a
`github:owner/repo` document ACL — a private-repo doc scoped to that group is
readable only by a principal carrying it.

### Conversations are scoped per user, not just per org

Document ACLs are one dimension; **conversation history is another**. Org
scoping alone is not access control here — every member of an org shares one
org id, so an org-only filter lets any authenticated member enumerate and open
every other member's conversations.

So `list_conversations`, resume-by-`conversationId`, and
`get_conversation_messages` are ALSO scoped to the connection's principal, by
its **`email` claim**, matched against the conversation's owning `user`
participant (`StorageAdapter::list_conversations_by_org_and_user`; Postgres
pushes it into the query, other adapters filter participants).

The rules, in order:

| connection | conversation reads |
| --- | --- |
| auth **disabled** (`AUTH_MODE=none`, unconfigured, or the single-user `local-token` daemon) | **unscoped** — no identity concept exists. The only unscoped case. |
| auth **enabled**, principal with an `email` | scoped to that email (on top of the org scope) |
| auth **enabled**, no principal or no `email` claim | **fails closed** — empty list, every read is not-found. Never falls back to the org. |

Two properties worth keeping in mind if you touch this path:

- **The principal wins over the frame.** `create_conversation_session` accepts a
  `userEmail` field; it is caller-controlled, so an authenticated connection
  always stamps the *principal's* email instead. Otherwise anyone could mint a
  conversation under someone else's identity — or read one back.
- **Not-yours is indistinguishable from not-found.** Reading another user's
  session returns the byte-identical `SESSION_NOT_FOUND` a session id that never
  existed returns, and resuming another user's conversation mints a fresh one
  exactly as an unknown id does. A distinct "forbidden" would be an existence
  oracle: it confirms which ids are real, which is all enumeration needs.

### Bring-your-own auth — the JWT contract

smooth-operator does **not** require you to adopt its identity provider. Point it
at your own auth (SSO/IdP) and have *that* mint the access tokens. The server only
needs a key to verify them and a handful of claims:

**Server config (env):**

| Var | Value |
| --- | ----- |
| `AUTH_MODE` | `jwt` |
| `AUTH_JWT_RS256_PUBLIC_KEY` *or* `AUTH_JWT_HS256_SECRET` | your IdP's verification key (RS256 PEM, or an HS256 shared secret) |
| `AUTH_JWT_ISSUER` / `AUTH_JWT_AUDIENCE` | *(optional)* enforce `iss`/`aud` when set |

**The JWT your IdP issues** (signed with the key above):

```jsonc
{
  "sub":    "u_123",                              // → user_id (matches DocAcl.users)
  "org":    "topstep",                            // → org_id (org isolation); `org_id` also accepted
  "role":   "basic",                              // admin | curator | basic (admin-API RBAC)
  "groups": ["github:topstep/svc-pricing",        // → the entitlements that gate document access;
             "github:topstep/svc-orders"],        //   a doc scoped to a group is readable only by a carrier
  "email":  "ada@topstep.com",                    // → the per-user CONVERSATION scope (see below)
  "exp":    1750000000                            // required
}
```

That's the whole contract: **`sub` + `org` + `role` + `groups` + `email` + `exp`.** Your IdP
decides which `groups` each user carries (e.g. map your SSO groups / GitHub team
membership to `github:owner/repo` strings) — the server enforces them at retrieval
with zero additional code. The widget/clients send the token on the `/ws` `?token=`
query param (reference server) or the `send_message` `token` field (Lambda); see
[`/ws` authentication](#ws-authentication--accesscontext) above. No token ⇒
anonymous ⇒ org-public only (fail closed).

> **Matching the `groups` claim to document ACLs — direct SSO mapping.** A document
> is readable only by a principal whose `groups` claim contains one of the
> document's ACL group strings. So the values in `groups` must match the strings
> the connector stamped: either the connector's configured **`acl_groups`** (when
> the operator set custom group names — see
> [[Connectors#acl_groups--configurable-group-naming-map-your-sso-groups-directly|CONNECTORS.md → `acl_groups`]])
> or the **default** `github:owner/repo` string. Because `acl_groups` is stamped
> verbatim, you can wire an IdP group **directly** to a repo's ACL with **no
> translation layer**: set the GitHub connector's `acl_groups: ["TS-Eng-Pricing"]`
> to the same Okta group `TS-Eng-Pricing` your IdP puts in the user's `groups`
> claim, and **only** carriers of `TS-Eng-Pricing` can read that repo's documents:
>
> ```text
> Okta group  "TS-Eng-Pricing"          (your IdP membership)
>   → JWT      "groups": ["TS-Eng-Pricing", …]   (minted by your IdP)
>   → connector acl_groups: ["TS-Eng-Pricing"]   (stamped verbatim on the repo's docs)
>   ⇒ only users carrying TS-Eng-Pricing retrieve topstep/svc-pricing's documents.
> ```

### Mint the token for the direct-widget case (JWT)

When the **widget connects directly** to smooth-operator (it can reach `/ws`),
your backend mints a **short-lived signed JWT** and hands it to the widget as the
`?token=`. ~12 lines of Node/TS, HS256 over a secret shared with the server
(`AUTH_JWT_HS256_SECRET`):

```ts
import jwt from 'jsonwebtoken';

// On YOUR backend, after you've authenticated the user. Never ship the secret to
// the browser — mint the token server-side and hand only the token to the widget.
export function mintSmoothOperatorToken(user: {
    id: string;
    org: string;
    role: 'admin' | 'curator' | 'basic';
    groups: string[];
}): string {
    return jwt.sign(
        { sub: user.id, org: user.org, role: user.role, groups: user.groups },
        process.env.AUTH_JWT_HS256_SECRET!, // shared with the server's AUTH_JWT_HS256_SECRET
        { algorithm: 'HS256', expiresIn: '5m' }, // short-lived; `exp` is required + enforced
    );
}
// → hand the returned string to the widget as `wss://…/ws?token=<jwt>`.
```

The server **verifies** this signature + `exp` on every connect. This is the
**direct/signed** path. The **proxied/no-token** path is `AUTH_MODE=trusted`
below — pick one based on whether clients can reach smooth-operator directly.

### Tokenless: `AUTH_MODE=trusted` (proxied integration)

Use this when you're embedding smooth-operator into an **existing app whose
backend already authenticated the user** and **proxies** the WebSocket to
smooth-operator over a **trusted/internal network**. Your backend already knows
who the user is — there's no second token to mint or verify. It simply
**forwards the identity** and smooth-operator **trusts it**.

**When to use it**

- ✅ Your backend authenticates the user, then proxies `/ws` (or the Lambda
  `send_message` frames) to smooth-operator on a private network the client
  cannot reach.
- ❌ **Do not** use it if clients can reach smooth-operator's `/ws` directly —
  use `AUTH_MODE=jwt` (signed, verified) for that. See the security boundary
  below.

**Server config (env)**

| Var | Value |
| --- | ----- |
| `AUTH_MODE` | `trusted` |

That's the whole config — there is **no key**, because there is **nothing to
verify**. The upstream owns identity *and* token lifetime, so there is no
signature check and **no `exp` requirement**. At startup the server logs a loud
warning that identity is trusted without verification.

**The forwarded identity** rides in the **same slot a JWT would** (the `?token=`
query param on the reference server, the `send_message` `token` field on the
Lambda) — so all the existing transport plumbing is reused unchanged. The value
is **`base64url(JSON)`** of the same claim shape (so it survives the
query-param / JSON-string transport cleanly without escaping):

```jsonc
// base64url( JSON.stringify(  ← what your proxy puts in the ?token= slot
{
  "sub":    "u_123",                       // → user_id (matches DocAcl.users)
  "org":    "topstep",                     // → org_id (org isolation); `org_id` also accepted
  "role":   "basic",                       // admin | curator | basic
  "groups": ["github:topstep/svc-pricing"] // → entitlements that gate document access
  // NOTE: no `exp` needed — the upstream owns lifetime.
}
// ) )
```

Minting it in the proxy (Node/TS):

```ts
function forwardSmoothOperatorIdentity(user: {
    id: string; org: string; role: string; groups: string[];
}): string {
    const claims = { sub: user.id, org: user.org, role: user.role, groups: user.groups };
    return Buffer.from(JSON.stringify(claims)).toString('base64url');
}
// → proxy the upstream connection to smooth-operator with `…/ws?token=<blob>`.
```

**Security boundary — trust without verification.** `AUTH_MODE=trusted` is
**only safe when clients cannot reach smooth-operator directly** — it must be
fronted by your authenticated backend/proxy on a trusted network. A client that
*can* reach `/ws` directly could **forge any identity** (any org, any groups).
This is the single most important constraint of the mode; the server emits a
startup `tracing::warn!` to that effect whenever `AUTH_MODE=trusted` is selected.

**Fail closed.** An **absent / empty / malformed** forwarded identity resolves to
`AccessContext::anonymous()` (org-public only) — **exactly** the no-token path.
Trusted mode **never** silently becomes a no-auth admin: a blob that fails to
decode, isn't claims JSON, or omits `role`/`org` is an error that degrades to
anonymous, never to an all-access principal. `jwt` / `smoo` / `none` and the
secure-by-default unset boot (admin-disabled) are all unchanged — `trusted` is
only ever reached by an explicit `AUTH_MODE=trusted`.

## Durable persistence (Postgres + DynamoDB)

The in-memory ACL side table dies with its process. The durable backends persist
the `DocAcl` **with the document** so the ACL survives ingest(process)→serve(process):

- **Postgres** — a `knowledge_vectors.acl JSONB` column, written at ingest from
  the `acl_v2` metadata. `query_async` filters **in SQL**: a row is visible when
  `acl IS NULL` (org-public) OR `acl->>'public'` is true OR the requester's user
  id is in `acl->'users'` (jsonb `?`) OR any requester group is in `acl->'groups'`
  (jsonb `?|`). The column is added idempotently (`ADD COLUMN IF NOT EXISTS`) so
  an in-place upgrade picks it up.
- **DynamoDB** — an `acl` string attribute on each knowledge item; the
  brute-force scan parses it back and post-filters via `can_access`.

"No ACL recorded ⇒ org-public" holds identically across all three backends.

## The model

### `DocAcl` — the document's allow-list

```rust
pub struct DocAcl {
    pub public: bool,        // visible to anyone reaching it
    pub users:  Vec<String>, // user ids explicitly allowed
    pub groups: Vec<String>, // group ids explicitly allowed
}
```

A document is **visible** to a requester when **any** of:

- `public == true`, or
- the requester's `user_id` ∈ `users`, or
- any of the requester's `groups` ∈ `groups`.

`DocAcl` serializes to JSON and rides in the document metadata under the key
`acl_v2` (`DocAcl::ACL_METADATA_KEY`). `DocAcl::attach_to(doc)` stamps it on;
`DocAcl::from_metadata(&doc.metadata)` reads it back. A **malformed** stamp
parses as "absent" (falls back to the default) so a corrupt value can't silently
lock or unlock a document.

### `AccessContext` — the requester's identity

```rust
pub struct AccessContext {
    pub user_id: Option<String>, // None for anonymous / system
    pub groups:  Vec<String>,
}

ctx.can_access(&acl) -> bool   // the gate
```

Built upstream from the authenticated user + their resolved group memberships.

## No-ACL default semantics — **no-acl ⇒ org-public**

This is the load-bearing backward-compatibility choice:

- A document ingested **without** an ACL (the legacy / existing-seed path) has
  **no entry** in the side table and is treated as **org-public** — visible to
  anyone whose query reaches it. Org isolation already happened upstream. This
  keeps all existing seeded knowledge retrievable; ACLs are strictly additive,
  opting a document *into* restriction.
- An **explicit** `DocAcl::default()` (`public: false`, empty `users`/`groups`)
  is the opposite: a fully-locked document only its listed users/groups can read.

So "no ACL recorded at all" (org-public) and "an empty ACL recorded"
(fully-locked) are deliberately different states.

## Over-fetch then filter

Filtering happens **after** the inner backend ranks results, so naively asking
the backend for `K` and then dropping the inaccessible ones would under-fill the
top-`K`. The reader instead **over-fetches**: it queries the inner backend for
`max(K * 5, 20)` candidates, filters by `can_access`, and truncates to `K`. So
the post-filter top-`K` stays full whenever enough accessible documents exist.
This mirrors the over-fetch the Postgres backend already does to feed RRF fusion.

## Wiring it into retrieval

`AccessContext` is threaded into **both** retrieval paths so neither can leak:

- **`KnowledgeChatRuntime::with_access_control(store, context)`** — when set,
  every turn reads knowledge through an `AccessContext`-bound reader. That one
  reader feeds both (a) the engine's auto-injected `[Relevant knowledge]`
  context (`AgentConfig::with_knowledge`) and (b) the `knowledge_search` tool —
  so the model never sees a restricted snippet through either path. Without it,
  the runtime reads the raw `storage.knowledge()` exactly as before
  (backward-compatible).
- **`KnowledgeSearchTool::with_access_control(&store, context)`** — builds the
  tool directly bound to a requester, for callers wiring tools by hand.

Ingestion stamps ACLs automatically: the pipeline's `RawDocument.acl` labels are
written as a `DocAcl` (interpreted as **group** entitlements — the common
connector-permission shape) under `acl_v2`, in addition to the legacy
comma-joined `acl` field kept for debug visibility. See
[[Ingestion Pipeline]].

## Tests

`smooth-operator/tests/access_control.rs`:

- **The cross-user leak test** (written first, failed before enforcement
  existed): three docs share a query term — `doc-a` (alice-only), `doc-b`
  (bob-only), `doc-pub` (public). Querying the shared term as bob returns `doc-b`
  + `doc-pub` and **never** `doc-a`; symmetric for alice.
- A **group** case: a doc visible to group `support` is seen by a member and
  hidden from a non-member.
- A **backward-compat** case: a no-ACL doc stays retrievable by an anonymous
  requester (org-public default).
- An **end-to-end runtime** case: a turn run *as bob* through
  `KnowledgeChatRuntime` + the `knowledge_search` tool never surfaces alice-only
  content in the tool result the model reads.

Plus a `can_access` unit-test matrix in `src/access_control.rs` (public,
user-match, user-no-match, group-match, group-no-match, empty-acl fully-locked,
mixed user-or-group) and `DocAcl` metadata round-trip / malformed-is-absent
tests.

### Chat-path + persistence + cross-org tests (the live-path hardening)

- **The headline chat-path leak test** — `smooth-operator-server/tests/acl_chat_leak.rs`
  (written first, failed before the runner threaded an `AccessContext`). It runs
  the **real** `run_streaming_turn` offline (a `MockLlmClient` scripts the
  streaming `knowledge_search` call) over an in-memory store seeded with an
  org-public doc and a private-repo doc scoped to group `github:acme/secret`,
  and asserts: a user **without** the group (and an anonymous connection) never
  see the private doc in the tool result the model reads **or** in any citation;
  a user **with** the group does.
- **Postgres persistence** — `adapters/postgres/tests/acl_persistence.rs`
  (testcontainers): ingest an ACL'd doc through one adapter, then query through a
  **fresh** adapter (a different process, in production) → the ACL is enforced
  from the `acl` column, proving it survives the ingest→serve boundary.
- **Groups-from-JWT** — `src/auth.rs` unit tests: a token's `groups` claim
  surfaces on the `Principal` and its `AccessContext`, and a tokenless principal
  cannot match a group-scoped doc.
- **Cross-org admin scoping** — `smooth-operator-server/tests/admin_api.rs`:
  org A's indexing runs + document sets are invisible to an org-B caller, and
  two orgs with a same-named connector don't collide (see [[Admin API]]).

## Related

- [[Storage Adapters]] — the `StorageAdapter` seam and the knowledge slice.
- [[Ingestion Pipeline]] — where `RawDocument.acl` is stamped into `acl_v2`.
- [[Feature Gaps]] — G3 and the TDD plan.
