---
"@smooai/smooth-operator": minor
---

SMOODEV-3292: the turn's `AccessContext` carries the session's agent, so a host can narrow knowledge retrieval to what that agent is configured to use.

- **Rust: `AccessContext::agent_id`** (`Option<String>`) plus the `with_agent_id` builder. The server's `send_message` handler and the Lambda dispatcher stamp it from the session's `agent_id` on every turn, on both the anonymous (widget) and the authenticated path.
- `knowledge_for_access` previously received only `user_id`, `groups` and `organization_id`, so a host could not tell which agent a turn belonged to. A multi-tenant host whose agents are each limited to a subset of the org's knowledge therefore grounded a restricted public agent on the whole org's knowledge.
- The built-in ACL ignores the field, so nothing changes for a host that does not read it. Code that builds `AccessContext` with a struct literal must add `agent_id` (or use `..Default::default()`). Rust-first: no other language has a host knowledge seam.
