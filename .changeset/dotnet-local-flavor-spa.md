---
"@smooai/smooth-operator-server-dotnet": minor
---

C# server local flavor: serve a prebuilt SPA same-origin from `SMOOTH_WEB_DIR` with the local token injected into `index.html` as `window.__SMOOTH_TOKEN__`, a `SMOOTH_LOCAL_TOKEN` → `LocalTokenVerifier` for same-origin `/ws` auth, and `SMOOTH_PERSONA` to set the agent's system prompt. Lets the .NET server be a drop-in "Big Smooth" backend behind the shared smooth-web Presence UI (validated end-to-end: SPA + WS + streamed persona reply).
