---
"@smooai/smooth-operator": minor
---

feat: thread `organization_id` into `AccessContext` for per-turn knowledge scoping

`StorageAdapter::knowledge_for_access(&self, access)` carried only `user_id` +
`groups` — no org — so a multi-tenant relational backend (SmooAI) could not scope
RAG to the turn's organization and was forced to a single static org. This was the
last multi-tenant gap on the knowledge path.

`AccessContext` now carries an additive `organization_id: Option<String>`
(default `None`, set via the new `with_organization_id(...)` builder). The
authenticated-principal path (`Principal::access_context`) stamps the principal's
org automatically; the reference server / lambda send-message paths fall back to
the turn's **session** org (every session carries `organization_id` since the
create-session path derives it) when the requester has no org of its own. The org
is then **available** to a host adapter's `knowledge_for_access` so it can scope
retrieval to the right tenant.

The operator's built-in single-tenant ACL ignores the org (org isolation already
happens upstream), so this is behavior-preserving for the reference/local flavor.
The Postgres knowledge adapter additionally uses the context's org — when present
— to **override** its construction-time org as a cheap SQL pre-filter
(`organization_id = $1`), so one adapter instance can serve per-turn tenants
instead of being pinned to a single static org; an org-less context leaves the
construction-time org unchanged.
