---
"@smooai/smooth-operator": patch
---

fix(rust): clear every clippy 1.96 failure in the workspace, so the gate can be turned on

Nine hard errors under `-D warnings` on clippy 1.96, all pre-existing and all
invisible because the clippy step is `continue-on-error`. Main goes red the moment
GitHub's stable runner reaches 1.96.

- **`unnecessary_sort_by` ×6** — `adapters/in-memory` (3) and `adapters/dynamodb` (3).
  `sort_by(|a, b| …cmp…)` → `sort_by_key`, with `Reverse` on the two newest-first
  sorts. Same ordering, no behavior change.
- **`too_many_arguments`** — `smooth-operator-server/src/protocol.rs::eventual_response`.
  Eight flat wire fields, one arg per emitted JSON key; given the same
  `#[allow]` its sibling builders in `handler.rs` and `server.rs` already carry,
  rather than a refactor that would touch all ten call sites for a lint heuristic.
- **`while_let_loop`** — a `loop` + `let … else { break }` in a test's socket read,
  now the `while let` clippy asked for.
- **`derivable_impls`** — `AuthMode`'s hand-written `Default` is now `#[derive(Default)]`
  + `#[default]`.

The last three only surfaced once the adapter errors were fixed: the build failed
before those crates were ever reached, so "fix the sort_by sites" and "clippy is
clean" were not the same job.

`cargo clippy --workspace --all-targets -- -D warnings` exits 0 on 1.96; 650 tests
pass; `cargo fmt --check` clean.
