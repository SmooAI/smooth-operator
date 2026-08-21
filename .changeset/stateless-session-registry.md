---
'@smooai/smooth-operator-server': minor
'@smooai/smooth-operator': minor
---

The session registry is no longer per-pod: `AppState` hydrates a session from
storage on a local miss, and `otpVerified` is persisted rather than kept in one
pod's memory (th-ca579c).

A visitor on smoo.ai asked the agent "do you do websites?" and got
`Error: session '<uuid>' not found` in the chat bubble. The widget's
returning-visitor resume POSTs `/internal/resume-by-fingerprint`, which primes
the session on whichever pod served that HTTP request, then opens a WebSocket —
which the load balancer sends to an arbitrary pod. The registry was
`Arc<RwLock<HashMap<String, Session>>>`, so the second pod had never heard of the
session and `scoped_session` reported it missing. With 2 replicas that is roughly
half of returning visitors; the HPA goes to 6.

`AppState::load_session` now falls back to `StorageAdapter::get_session` and
primes the local map, so any pod can serve any session and the map is a cache
rather than the source of truth. It is called from `scoped_session` — the
ownership check every session-bearing frame already passes through — which keeps
the synchronous readers (`session_authenticated`, `session_contact`,
`session_supports`) working untouched. A storage error is logged and returns
`None` rather than being reported as "no such session": a blip must not become an
existence claim that a human reads as an error.

`SessionUpdate` gains `metadata`, and the in-memory / Postgres / DynamoDB
adapters honour it. Without that field there was no way to write session metadata
back through the adapter at all, which is why `otpVerified` was memory-only:
a caller who completed OTP on one pod was silently unverified on the next frame
if the load balancer moved them, and after every roll — while the gate itself
worked exactly as designed.

**Why session metadata and not conversation metadata.** The workflow step pointer
(th-c12df5) and `supports` (th-13df6d) both moved to conversation metadata for
this same durability reason, so the precedent points there. `otpVerified` is
deliberately different: it is an authentication result, and conversation scope
would let any later session in the conversation inherit it. The consuming host
must also clear it on any resume that recognises a BROWSER rather than a person —
smooai's fingerprint resume has a 30-day TTL, and a fingerprint is a recognition
hint, never a credential.

The write is local-first then through to storage, and a storage failure is logged
rather than raised: the turn in flight has already verified the human, and
failing there would refuse service to someone who just proved who they are. The
cost is that the verification may not survive a pod hop — exactly the pre-fix
behaviour, so a degradation rather than a regression.

Still per-pod, and correctly so: `pending_confirmations` and
`pending_interactions` hold channel senders into a turn parked in that process.
Those cannot move to shared storage — the parked turn is the state — and making
them survive a pod switch is durable execution, not a shared map.
