---
name: polyglot-parity
description: >-
  Drive a feature or fix through all 5 smooth-operator implementations
  (Rust · C# · Python · TypeScript · Go) and prove parity with the shared
  conformance corpus. Use when changing the protocol/engine/server in this repo
  and the change must land in every language — triggers: "propagate to all
  languages", "keep the polyglot in parity", "update all 5 servers/SDKs",
  "port this to C#/Go/Python/TS", "did I miss a language".
---

# Polyglot parity

One change, five languages, one oracle. `spec/` is the source of truth; the
conformance corpus is what catches a language you forgot.

## Order (do not skip step 1)

1. **Spec first.** If the change touches the wire (a frame, field, event, error)
   edit `spec/` *before* any language: the schema under `spec/{events,domain,actions}/`
   and **add/extend a scenario** in `spec/conformance/scenarios/*.json`. The scenario
   is the contract — every server replays it and must emit identical output. No spec
   change for a pure engine-internal fix, but it still lands in all 5 (step 2).

2. **Apply to every language.** Same change, each native stack:

   | Lang | Server | Engine |
   |---|---|---|
   | Rust | `rust/smooth-operator-server/` | `rust/smooth-operator/` |
   | C# | `dotnet/server/` (`host/`, `aspnetcore/`, `src/`) | `dotnet/src/` |
   | Python | `python/server/` | `python/core/` |
   | TypeScript | `typescript/server/` | `typescript/core/` |
   | Go | `go/server/` | `go/protocol/` |

   Rust is the reference; mirror its surface. The Rust scenario test header
   (`rust/smooth-operator-server/tests/scenario_parity.rs`) names the sibling ports.

3. **Verify — run the parity oracle in each language.** Each replays
   `spec/conformance/scenarios/*.json` against its server; a diff = a bug in that
   language (or the corpus). Run all five:

   ```bash
   # Rust
   cargo test -p smooai-smooth-operator-server --test scenario_parity
   # C#
   dotnet test dotnet/server/integration-tests        # ScenarioParityTests.cs
   # Python
   cd python/server && uv run pytest tests/test_scenario_parity.py
   # TypeScript
   pnpm -C typescript/server vitest run test/scenario-parity.test.ts
   # Go
   cd go && go test ./server -run ScenarioParity
   ```

4. **Changeset per affected package** (`pnpm changeset`) and land per the repo's
   rules. The README "polyglot story" table is the up-to-date status of which
   servers carry the full surface vs. protocol-only.

## The one rule

A change isn't done when Rust is green — it's done when **all five parity tests
pass**. If you can only do one language now, add the scenario anyway: the other
four tests will then fail loudly until they're caught up, which is the point.
