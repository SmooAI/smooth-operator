---
"@smooai/smooth-operator": patch
---

fix(server): a storage blip is no longer reported as `session '<id>' not found`

`AppState::load_session` hydrates a session from storage when the local
per-pod registry misses (th-ca579c) — the normal path for a returning visitor
whose WebSocket lands on a pod that has never seen their session. That read
collapsed `Err` into `None`, so a transient Postgres failure was
indistinguishable from a session that genuinely does not exist. Every caller
renders `None` as `session '<id>' not found`, so a backend hiccup told a live
visitor on smoo.ai, in the chat bubble, that their conversation was gone. Seen
in production.

`load_session` now returns `anyhow::Result<Option<Session>>` and
`handler::scoped_session` propagates it. The three outcomes are distinct at the
user-visible boundary:

- `Ok(Some(session))` — unchanged.
- `Ok(None)` — not found, **or** not yours: still the identical
  `SESSION_NOT_FOUND` / `NO_PENDING_*` event, byte for byte, so there is no
  existence oracle to enumerate other users' session ids with.
- `Err(_)` — storage could not answer: a retryable `STORAGE_ERROR` ("session
  lookup is temporarily unavailable, please try again"), which is not an
  existence claim and leaks nothing (a storage failure is independent of
  whether the id is real or ours). The underlying error is logged server-side,
  not sent to the client.

All six session-id-taking actions route through the one chokepoint and switch
together: `get_session`, `get_conversation_messages`, `send_message` and
`verify_otp` (previously `SESSION_NOT_FOUND`), plus `confirm_tool_action` and
`submit_interaction` (previously `NO_PENDING_CONFIRMATION` /
`NO_PENDING_INTERACTION`, which for a parked turn was equally wrong — the park
is still there, and a retry still resolves it).

`rust/smooth-operator-server/tests/session_storage_blip.rs` drives the real
dispatcher against a storage adapter whose `get_session` fails on demand and
pins both halves: a blip is never rendered as not-found on any of the six
actions, and a genuinely unknown id still is (a fix that made everything
retryable would leave clients retrying an id that will never resolve).

**Host-facing API change**: `AppState::load_session` returns
`anyhow::Result<Option<Session>>` instead of `Option<Session>` — hosts calling
it directly add a `?` or a `match`.
