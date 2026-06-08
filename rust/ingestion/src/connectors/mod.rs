//! Built-in [`Connector`](crate::connector::Connector) implementations.
//!
//! Shipped:
//! - [`FileConnector`] — local `.txt`/`.md` files (file or directory).
//! - [`WebConnector`] — a public URL, HTML-stripped, SSRF-guarded.
//!
//! - [`GithubConnector`] — a GitHub repository's prose (READMEs / `docs/` /
//!   `*.md`), source code, and issues / PRs / discussions via the GitHub API.
//!
//! Follow-up (stubbed by design, tracked under Onyx-gap G1): the broader SaaS
//! set Onyx covers (confluence, jira, notion, slack, …). Each new connector is
//! a `Connector` impl plus a `unit` test against fixture data and an
//! `external`-gated live test, exactly like [`WebConnector`]'s split — see
//! `docs/CONNECTORS.md` § "Authoring a custom connector".

pub mod file;
pub mod github;
pub mod web;

pub use file::FileConnector;
pub use github::{
    GithubAuth, GithubConnector, GithubConnectorConfig, GithubInclude, GithubVisibility,
};
pub use web::WebConnector;
