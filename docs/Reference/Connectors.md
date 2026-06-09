# Connectors: GitHub

The **GitHub connector** pulls a repository's knowledge into the ingestion
pipeline, and a companion **`github_search`** tool gives the agent fresh, live
lookups beyond the indexed snapshot. Together they are the foundation of the
`examples/dev-support` dev-team knowledge agent.

- Connector: `rust/ingestion/src/connectors/github.rs`
  (`smooth_operator_ingestion::connectors::github`)
- Tool: `rust/smooth-operator/src/tools/github_search.rs`
  (`smooth_operator::tools::github_search`)

It is a normal [[Ingestion Pipeline#the-connector-trait|`Connector`]] impl: an offline
`unit` contract test (a **mock GitHub API** via `wiremock`) plus an
`external`-gated live test, exactly the [[Ingestion Pipeline#tests--the-unit-vs-external-split-g9|G9 split]]
every connector follows.

## Content types → `RawDocument`

The connector pulls three tiers of content, each as one or more `RawDocument`s
carrying rich metadata (`repo`, `path`, `url`, `kind`, `ref`, plus tier-specific
keys) so retrieval and citations can attribute every chunk back to GitHub.

| Tier      | What                                                                 | `metadata.kind`         | `source` (citation)    | Extra metadata        |
| --------- | -------------------------------------------------------------------- | ----------------------- | ---------------------- | --------------------- |
| **Prose** | `README*` anywhere, everything under `docs/`, and `*.md` / `*.mdx`   | `prose`                 | the GitHub **blob URL** | `path`, `url`, `ref`  |
| **Code**  | source files (extension allowlist), one `RawDocument` per file       | `code`                  | the GitHub **blob URL** | `path`, `lang`, `ref` |
| **Issues**| issues, PRs, and discussions — title + body + top comments concatenated | `issue` / `pr` / `discussion` | the GitHub **issue/PR URL** | `state`, `labels`, `number`, `updated_at` |

The pipeline chunker splits each file/issue; the connector emits one
`RawDocument` per file (code) or per issue, and the chunker propagates the
metadata onto every chunk.

> **GitHub URLs flow into citations.** The blob/issue URL stamped onto each
> document's `source` is what the runtime turns into a citation `url` on the
> terminal `eventual_response` (see [[Protocol Reference#citations-on-eventual_response|PROTOCOL.md]]).
> When the agent grounds an answer in a GitHub-sourced chunk, the returned
> `Citation` carries that blob/issue URL as its `url`, so a UI can link the
> answer straight back to the file or issue on GitHub. Documents whose `source`
> is not an `http(s)` URL (e.g. uploaded files with a plain path) simply omit
> `url`.

### Code filtering

Code files are filtered so the index stays high-signal:

- **Extension allowlist** (`DEFAULT_CODE_EXTENSIONS`, override per-config):
  `rs, ts, tsx, js, jsx, py, go, java, kt, rb, php, cs, c/c++, swift, scala, sh,
  sql, yaml, toml, json, html, css`, …
- **Skipped regardless of extension**: vendored / build-output trees
  (`node_modules/`, `vendor/`, `target/`, `dist/`, `build/`, `.git/`,
  `__pycache__/`, `.next/`, `venv/`), **lockfiles** (`package-lock.json`,
  `pnpm-lock.yaml`, `yarn.lock`, `Cargo.lock`, `go.sum`, `uv.lock`, …), and
  **binaries/assets** (`png`, `pdf`, `woff2`, `wasm`, `dylib`, …).
- **Size cap** (`max_file_bytes`, default 512 KiB): larger files are skipped, and
  non-UTF-8 (binary) blobs are rejected.

The filter is a pure function (`classify_path`) with offline unit tests.

## Configuration

```rust
use smooth_operator_ingestion::connectors::github::{
    GithubAuth, GithubConnector, GithubConnectorConfig, GithubInclude, GithubVisibility,
};

let config = GithubConnectorConfig::new("smooai", "smooth-operator", GithubAuth::Token(token))
    .at_ref("main")                              // None → the repo's default branch
    .include(GithubInclude { prose: true, code: true, issues: true })
    .code_extensions(["rs", "ts", "py"])          // override the allowlist
    .max_file_bytes(256 * 1024)
    .visibility(GithubVisibility::Private)         // private → stamp an ACL
    .acl_groups(["eng-team"]);

let connector = GithubConnector::new(config);
let docs = connector.pull(None).await?;           // or pull(Some(since)) — incremental
```

`GithubConnectorConfig` fields: `owner`, `repo`, `auth`, `include` (prose / code /
issues toggles), `ref` (branch/tag/sha), `code_extensions`, `max_file_bytes`,
`visibility`, `acl_groups`, `base_uri` (tests point this at the mock server), and
`max_pages` (pagination safety cap).

## Auth — `GithubAuth`

```rust
pub enum GithubAuth {
    Token(String),                                  // a personal-access token (PAT)
    AppInstallation { app_id, private_key, installation_id },  // a GitHub App
    Unauthenticated,                                // public repos / the mock test
}
```

- **`Token(pat)`** — the simplest **self-host** path: bring your own personal-access
  token scoped to the repos you want indexed.
- **`AppInstallation { … }`** — a **GitHub App installation** (app id + PEM private
  key + installation id). This is how **lom.smoo.ai** wires it: Smoo owns **one**
  first-party GitHub App, and each customer **installs** it on their org so Smoo can
  index their repos **without anyone sharing a PAT**. The connector builds an app
  JWT from the PEM key and scopes down to the installation. A self-hosted
  deployment can use either path — Smoo-powered (the App) or BYO (a PAT/your own
  App).
- **`Unauthenticated`** — no creds (public repos, the offline contract test);
  subject to GitHub's anonymous rate limit in production.

> `GithubAuth`'s `Debug` impl redacts token and private-key material — secrets
> never reach logs.

### ACL — repo visibility maps to a `DocAcl`

Repo **visibility** drives the document ACL the pipeline stamps (see
[[Access Control]]):

- **Public** repo → documents are org-public (no ACL). Setting `acl_groups` on a
  public repo is a no-op — `acl_groups` only changes _which_ groups gate a
  **restricted** (private) repo; it never makes a public repo private (or
  vice-versa).
- **Private** repo → documents are scoped to `acl_groups` (a group entitlement); a
  private repo configured with **no** groups falls back to a synthetic per-repo
  group (`github:owner/repo`) so it is **never** accidentally org-readable.

The pipeline turns the `RawDocument.acl` labels into a `DocAcl` (under the
`acl_v2` metadata key) that ACL-filtered retrieval enforces at read.

#### `acl_groups` — configurable group naming (map your SSO groups directly)

`acl_groups` is the **exact set of group strings** stamped on a restricted repo's
documents — and those strings are matched **verbatim** against the requester's
`groups` claim at retrieval (see the BYO-auth section of
[[Access Control]]). That means an operator can stamp **their
own** IdP/SSO entitlement group names on a repo, so a customer's SSO groups gate
its documents **directly, with no translation layer**:

```rust
// An operator whose Okta directory has the group `TS-Eng-Pricing` can gate the
// pricing service's repo on it directly:
let config = GithubConnectorConfig::new("topstep", "svc-pricing", auth)
    .visibility(GithubVisibility::Private)
    .acl_groups(["TS-Eng-Pricing"]);   // ← your Okta group name, stamped verbatim
```

Now only a user whose forwarded JWT `groups` claim carries `TS-Eng-Pricing` can
retrieve `topstep/svc-pricing`'s documents — the Okta group **is** the document
ACL, end to end.

| `acl_groups`                  | visibility | stamped `DocAcl` groups       | who can read                          |
| ----------------------------- | ---------- | ----------------------------- | ------------------------------------- |
| `["TS-Eng-Pricing"]` (custom) | Private    | `["TS-Eng-Pricing"]`          | carriers of the `TS-Eng-Pricing` claim |
| _unset_ (default)             | Private    | `["github:owner/repo"]`       | carriers of the `github:owner/repo` claim |
| anything                      | Public     | _none_                        | the whole org (org-public)            |

Unset `acl_groups` ⇒ exactly the prior behavior (`github:owner/repo`), so this is
fully backward-compatible.

## Incremental pulls

`pull(Some(since))` passes `since` to the GitHub **issues** API's `since` filter
(only issues/PRs updated at/after the watermark are returned) and carries each
document's `updated_at` through metadata. Content-level idempotency is handled
downstream by the pipeline's `(id, content-hash)` [[Ingestion Pipeline#idempotency|ledger]],
so a full re-pull of unchanged files stores nothing new regardless.

## Rate limits & pagination

The connector paginates list calls (`per_page` + a `max_pages` safety cap) and
maps a GitHub **403 rate-limit** response to a clear error rather than an opaque
failure. The `octocrab` client handles auth + retry/backoff.

## The `github_search` tool (live, fresh lookups)

Where the connector indexes a **snapshot**, `github_search` hits the **live**
GitHub search API so the agent can find code or issues that landed *after* the
last ingest.

```jsonc
// arguments
{ "query": "fn parse_proxy_response", "kind": "code" }   // kind: "code" | "issues"
```

- Constructed with a `GithubAuth` + a default `owner/repo` scope; the scope is
  folded into the query as `repo:owner/name` (unless the query already pins a
  `repo:`/`org:`/`user:` qualifier), so lookups stay within the team's repos by
  default.
- `kind = "code"` searches source (`/search/code`); `kind = "issues"` searches
  issues + PRs. Returns top results as **title · URL · snippet**.
- Registered separately from the default `builtin_tools` catalog (it needs an
  explicit auth + scope). Wire it with
  `GithubSearchTool::new(auth, owner, repo)`.

```rust
use smooth_operator::tools::github_search::{GithubAuth, GithubSearchTool};

let tool = GithubSearchTool::new(GithubAuth::Token(token), "smooai", "smooth-operator");
```

The live network sits behind the pluggable `GithubSearchBackend`
(default: `OctocrabGithubSearch`); the tool's **arg-parsing + result-formatting
are unit-tested offline against a stub backend**, and the live path is an
`#[ignore]` + `SMOOTH_AGENT_E2E=1`-gated test — the same split as `web_search`
(see [[Tools]]).

## Tests

```bash
cd rust
# unit tier (mock GitHub API, no creds) — runs every PR:
cargo test -p smooai-smooth-operator-ingestion --test github_connector
cargo test -p smooai-smooth-operator github_search

# live tier (real GitHub) — nightly / on demand:
SMOOTH_AGENT_E2E=1 GITHUB_TOKEN=ghp_… \
  cargo test -p smooai-smooth-operator github_search::tests::live_search -- --ignored --nocapture
```

The contract test (`rust/ingestion/tests/github_connector.rs`) stands up a
`wiremock` GitHub API (repo tree + README + a source file + an issue), points
`octocrab` at it via `GithubConnectorConfig::base_uri(server.uri())`, and asserts
the connector emits correctly-shaped prose / code / issue `RawDocument`s (right
`kind`, blob/issue-URL `source`, `lang`/`state`/`labels` metadata), that a
**private** config stamps a restricting ACL while a **public** one stays public,
and that the full `ingest(github, chunker, embedder, knowledge)` round-trip makes
a distinctive seeded term retrievable.

## How `octocrab` is pointed at the mock

`octocrab` 0.53 exposes `OctocrabBuilder::base_uri(...)`; the connector threads
`config.base_uri` into it, so the contract test sets it to the local `wiremock`
server's URL and every GitHub call is served offline. (The connector also
installs the rustls `ring` crypto provider once before the client's first TLS
use, because the workspace graph carries both `ring` and `aws-lc-rs` and rustls
0.23 can't otherwise auto-pick a provider.)

## Related

- [[Ingestion Pipeline]] — the pipeline, the `Connector` trait, chunker,
  embedder, idempotency, and the G9 test split.
- [[Access Control]] — how `RawDocument.acl` → `DocAcl` →
  ACL-filtered retrieval.
- [[Tools]] — the `Tool` shape and the built-in tool catalog.
