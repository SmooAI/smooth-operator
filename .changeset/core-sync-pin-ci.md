---
'@smooai/smooth-operator': patch
---

th-6784a6 — sync to core@main + pin the CI core checkout so a moving core can't
silently break every PR.

`pr-kind-deploy-smoke.yml` checked out `SmooAI/smooth-operator-core` with no
`ref`, so when core@main advanced (multimodal `Message.images` field), this
repo's `main` stopped compiling against it and `cargo build --locked` failed the
lock check — turning every open PR red for reasons unrelated to its own diff.

- Add `images: vec![]` to the two `EngineMessage` constructions (replayed
  text-only history) in `runtime.rs` and `runner.rs`.
- Fix stale test literals missing new struct fields: `suggested_replies.rs`
  (`identity_intake` → `interactions`, removed in #176) and `serve_smoke.rs`
  (`ServerConfig` + `TurnRequest` new fields).
- Regenerate `Cargo.lock` against core@main so `--locked` passes.
- Pin the CI core checkout to a known-good SHA
  (`3c7b21dbde4f31519b2eab3d5343f154119fe655`), documented as interim until
  core publishes to crates.io. Bump it deliberately alongside
  `cargo update -p smooai-smooth-operator-core`.
