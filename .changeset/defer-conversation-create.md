---
"@smooai/smooth-operator": patch
---

fix(server): a conversation is born on its first message, not on the widget open (SMOODEV-3057)

Opening the widget wrote a `conversations` row — plus both participants and a
session — before the visitor had typed anything. In a 30-day production sample
**44 of 117** web conversations carried zero messages: bare opens occupying an
inbox row. And because a web create feeds a fresh UUID as the conversation's
`idempotency_key`, the unique index's `ON CONFLICT DO NOTHING` could never
collapse a double-connect the way it does for sms/slack/discord, so a
reconnecting visitor accumulated rows.

`create_conversation_session` now parks a **bare** open — no `userEmail`, no
`metadata.userPhone`, no `conversationId` to resume — and writes it on the
session's first `send_message`, in the same order as before (conversation → user
participant → agent participant → session). A connection that closes without a
message drops its parked writes; a reconnect naming the parked `conversationId`
binds to it, keeping the id and the durable `supports` record.

The wire is unchanged: the client still gets its `sessionId` / `conversationId`
back immediately, and the session is usable on that connection at once.

An open that **carries visitor identity** is not deferred. It is a captured lead,
and a host adapter may hook the `user` participant write to upsert that visitor
into its CRM (reading phone + marketing consent off the conversation's
`metadata_json`), so those still persist immediately — as do every non-web
channel and every resume.

A storage blip during the flush is reported as retryable `STORAGE_ERROR` with the
parked writes kept, and the retry resumes at the step it stopped on rather than
re-inserting a row whose primary key is already taken.
