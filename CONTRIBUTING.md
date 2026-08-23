# Contributing to smooth-operator

Thanks for being here. This file is the short version of how this repo actually works — the parts
that will get a PR sent back if you skip them. It is deliberately specific to smooth-operator rather
than generic advice.

**Where things go** — the last one is the rule people get wrong, and getting it wrong publishes a zero-day:

- **Bug or concrete task** → [an Issue](https://github.com/SmooAI/smooth-operator/issues)
- **Question, idea, or "how do I…"** → [a Discussion](https://github.com/SmooAI/smooth-operator/discussions), where it stays searchable for the next person who asks
- **Security vulnerability** → the private channel in [SECURITY.md](./SECURITY.md) ([dev@smoo.ai](mailto:dev@smoo.ai)), **never** either of the public two

## The four things that matter

### 1. Add a changeset, or your work is merged but never published

This is the one that bites hardest, because nothing fails loudly: your PR goes green, it merges, the
release workflow succeeds, and your change is simply not in any published artifact.

Every artifact here — npm, NuGet, PyPI, crates.io, the Go module — ships at **one shared version**,
and that version lives in exactly one package: **`@smooai/smooth-operator`**
(`typescript/package.json`). Changesets natively versions only npm packages, so
`scripts/sync-versions.mjs` stamps that number onto every other manifest. The consequence people
miss: **a changeset that does not name `@smooai/smooth-operator` cannot republish any non-npm
artifact.** One changeset naming the anchor covers all five languages.

```bash
pnpm changeset
```

Then, in the generated file, make sure the frontmatter names the anchor:

```md
---
'@smooai/smooth-operator': minor
---

feat(python): what changed and why it matters
```

Sitting one word away is `@smooai/smooth-operator-server` — the *TypeScript* server on npm. If you
just changed the .NET server, that name is the intuitive pick and the wrong one. PR #348 and PR #352
both landed .NET features that stayed unpublished for days for exactly this reason.

CI enforces it: the **Anchor Guard** workflow (`.github/workflows/anchor-guard.yml`,
`scripts/check-changeset-anchor.mjs`) fails your PR if it touches a lockstep-stamped tree and carries
a changeset that does not name the anchor. It deliberately does *not* fire when a PR has no changeset
at all — docs-only and test-only PRs legitimately have none. The bug it exists to catch is not "no
changeset", it is "a changeset naming the wrong package".

Write the changeset body like a changelog entry someone will read six months from now: what changed,
why, and what it means for a consumer. Look at the existing files in `.changeset/` for the register.

### 2. Rust is the reference implementation; the ports mirror it

The Rust implementation (`rust/smooth-operator`, `rust/smooth-operator-server`, `rust/adapters/*`)
is the source of truth for behaviour. TypeScript, Python, Go, and .NET are held to **behavioral
parity** with it, enforced by tests rather than by mirroring type shapes — each language stays
idiomatic.

So a behaviour change starts in Rust:

1. Change the Rust behaviour and its test.
2. If the **wire** changes, change the JSON Schemas in `spec/` too (see §3).
3. Port the behaviour to the other languages, each with its own parity test named/scoped to its Rust
   counterpart so a gap stays visible.

A port-only PR (bringing one language up to a behaviour Rust already has) is very welcome and does
not need the Rust step — that behaviour already exists. What we want to avoid is a behaviour that
exists *only* in a port, because then there is no reference to check the others against.

`CLAUDE.md` has the longer version of this, including the layer table (engine / service / client)
and how the eval scenarios fit in. `docs/Architecture/Polyglot Cores.md` has the parity contract in
full.

### 3. `spec/` is the wire protocol's source of truth, and generated types are committed

`spec/` holds language-neutral JSON Schemas for the WebSocket protocol — `envelope.schema.json`,
`actions/`, `events/`, `domain/`, `interactions/` — with canonical instances in
`spec/conformance/fixtures.json`.

Every language generates its protocol types from those schemas, and **the generated files are
committed**:

| Language | Generated file |
| --- | --- |
| TypeScript | `typescript/src/generated/types.ts` |
| Go | `go/protocol/types_gen.go` |
| Python | `python/src/smooth_operator/_generated.py` |
| .NET | `dotnet/src/Generated/Types.cs` |

Committed means reviewable — but it also means it is possible to hand-edit one, and a hand-edit is
drift that no schema check will catch. **Change the schema and regenerate**; never patch a generated
file directly.

```bash
scripts/generate-go.sh                                  # Go
pnpm --filter @smooai/smooth-operator generate          # TypeScript
```

Python and .NET generator invocations live in [`spec/codegen/README.md`](./spec/codegen/README.md).

### 4. Tests: run them, and prove the new one fails first

A new test should be **shown to fail without its fix**. Write it, watch it go red, then implement
until it is green. A test that passes before the change is not testing the change — it is decoration
that will keep passing when the behaviour regresses.

Per language, matching what CI runs:

```bash
# Rust
cd rust && cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace

# TypeScript
pnpm install --frozen-lockfile
pnpm --filter "@smooai/smooth-operator" --filter "@smooai/smooth-operator-server" --filter "@smooai/smooth-extension-sdk" --filter "@smooai/create-smooth-extension" test

# Python (from the relevant project dir, e.g. python/)
uv sync && uv run ruff check . && uv run ruff format --check . && uv run pytest

# Go (from the relevant module dir, e.g. go/)
gofmt -l . && go vet ./... && go test ./... -race

# .NET
dotnet test dotnet/SmooAI.SmoothOperator.slnx --configuration Release
```

Unit, parity, and conformance tests run in CI with **no credentials** and must be green. Live tests
that hit a real gateway or LLM judge are gated behind `SMOOTH_AGENT_E2E=1` + `SMOOAI_GATEWAY_KEY`
and must **skip cleanly** when those are absent — a test that fails for a missing credential trains
everyone to ignore red CI.

Don't land a language change without its parity tests.

## Pull requests

- Branch off `main`, keep the PR focused on one thing.
- Say what changed and **why**. The why is the part reviewers cannot reconstruct.
- Include the changeset (§1) unless the PR is genuinely docs- or test-only.
- Get CI green. If a check is red for a reason you believe is unrelated, say so in the PR rather
  than merging past it.
- New behaviour needs a test that failed before your fix (§4).

## Code of conduct

By participating you agree to the [Code of Conduct](./CODE_OF_CONDUCT.md).

## Licence

Contributions are accepted under the [MIT Licence](./LICENSE), same as the project.
