---
'@smooai/smooth-operator': patch
---

Consume `smooai-smooth-operator-core` from crates.io (0.16) instead of the sibling
path dep, and collapse the image build to a single-repo Docker context.

- `rust/Cargo.toml`: `smooai-smooth-operator-core` path dep → `"0.16"` (published crate).
- `Dockerfile`: drop the sibling `smooth-operator-core` COPY; context is this repo alone (cargo fetches the engine crate from crates.io).
- `deploy/scripts/kind-smoke.sh`: build from the repo root, drop `PARENT_DIR`/`SIBLING_DIR`.
- `.github/workflows/pr-kind-deploy-smoke.yml`: drop the sibling checkout + `ref:` pin + `PARENT_DIR` env.

`Cargo.lock` regen + `cargo build --locked` verification happen AFTER 0.16.0 is
published to crates.io.
